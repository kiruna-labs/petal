import { test } from 'node:test';
import assert from 'node:assert/strict';

import { INVALID_JOIN_INPUT_ERROR, looksLikeJoinAttempt, parseJoinInput } from '@petal/shared/logic/joinInput';
import { accessCodeFromInviteInput, internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';

const ACCESS_CODE = 'abc-defg-hjk';
const PETAL_LIVE_CODE = 'joa-uozn-rxt';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

test('accessCodeFromInviteInput preserves the real code from pasted links', () => {
  assert.equal(accessCodeFromInviteInput(`https://meet.petal.live/petal-meeting/${ACCESS_CODE}`), ACCESS_CODE);
  assert.equal(accessCodeFromInviteInput(`petal://join/${ACCESS_CODE}`), ACCESS_CODE);
});

function expectCode(input: string, code = CREDENTIAL) {
  assert.deepEqual(parseJoinInput(input), { ok: true, code }, `input: ${JSON.stringify(input)}`);
}

function expectError(input: string, error?: string) {
  const result = parseJoinInput(input);
  assert.equal(result.ok, false, `input: ${JSON.stringify(input)} should be rejected`);
  if (!result.ok && error) assert.equal(result.error, error);
}

test('bare access codes are trimmed, normalized, and resolved', () => {
  expectCode(ACCESS_CODE);
  expectCode(PETAL_LIVE_CODE, internalCredentialForAccessCode(PETAL_LIVE_CODE));
  expectCode(`  ${ACCESS_CODE.toUpperCase()}  `);
  expectCode('abcdefghjk');
});

test('petal://join/<access-code> links parse and URL-decode', () => {
  expectCode(`petal://join/${ACCESS_CODE}`);
  expectCode(`PETAL://JOIN/${ACCESS_CODE.toUpperCase()}`);
});

test('canonical https /<label>/<access-code> links parse and ignore cosmetic label', () => {
  expectCode(`https://petal.example.com/eng-sync/${ACCESS_CODE}`);
  expectCode(
    `https://meet.petal.live/petal-meeting/${PETAL_LIVE_CODE}`,
    internalCredentialForAccessCode(PETAL_LIVE_CODE)
  );
  expectCode(`https://petal.example.com/renamed-room/${ACCESS_CODE}`);
  expectCode(`http://localhost:5184/local-test/${ACCESS_CODE}?utm=ignored`);
});

test('relative /<label>/<access-code> paths parse like full invite links', () => {
  expectCode(`/eng-sync/${ACCESS_CODE}`);
  expectCode(`/${ACCESS_CODE}`);
});

test('legacy web-client links parse ?code= and #/join/ when they carry access codes', () => {
  expectCode(`https://petal.example.com/?code=${ACCESS_CODE}`);
  expectCode(`https://petal.example.com/#/join/${ACCESS_CODE}`);
});

test('label-only input and old public credentials are rejected', () => {
  expectError('eng-sync');
  expectError('eng-sync-0123456789abcdef0123456789abcdef');
  expectError('petal://join/eng-sync');
  expectError('https://petal.example.com/?code=eng-sync');
});

test('access-code typos are join attempts, not meeting names', () => {
  expectError('abc-defg-hi1', INVALID_JOIN_INPUT_ERROR);
  assert.equal(looksLikeJoinAttempt('abc-defg-hi1'), false);
  assert.equal(looksLikeJoinAttempt('Design Review'), false);
  expectError('myq-xfkw-azrp', INVALID_JOIN_INPUT_ERROR);
});

test('unrecognized shapes produce a clear error', () => {
  expectError('', 'Enter a meeting code or paste an invite link.');
  expectError('https://petal.example.com/app/name/abc-defg-hjk', INVALID_JOIN_INPUT_ERROR);
  expectError('ftp://weird.example.com/thing', INVALID_JOIN_INPUT_ERROR);
});
