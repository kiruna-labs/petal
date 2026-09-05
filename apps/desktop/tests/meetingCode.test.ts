import assert from 'node:assert/strict';
import test from 'node:test';

import {
  ACCESS_CODE_ALPHABET,
  accessCodeForCredential,
  buildMeetingInvitePath,
  generateAccessCode,
  generateMeetingCode,
  internalCredentialForAccessCode,
  looksLikeMeetingCredentialInput,
  meetingCredentialFromInviteInput,
  normalizeAccessCode,
  normalizeMeetingCredential
} from '../src/lib/data/meetingCode.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = 'room-8535e993a1b76ed8a9ee59b265f53dfc';
const PETAL_LIVE_CODE = 'joa-uozn-rxt';
const ACCESS_CODE_SHAPE_RE = /^[a-hjkm-z]{3}-[a-hjkm-z]{4}-[a-hjkm-z]{3}$/;

test('access-code alphabet excludes visually ambiguous lowercase i and l', () => {
  assert.equal(ACCESS_CODE_ALPHABET.includes('i'), false);
  assert.equal(ACCESS_CODE_ALPHABET.includes('l'), false);
  assert.equal(normalizeAccessCode('abc-defg-hij'), 'abc-defg-hij');
  assert.equal(normalizeAccessCode('abc-defg-hlj'), 'abc-defg-hlj');
});

test('normalizes Google-Meet-style access codes', () => {
  assert.equal(normalizeAccessCode(` ${ACCESS_CODE.toUpperCase()} `), ACCESS_CODE);
  assert.equal(normalizeAccessCode('abcdefghjk'), ACCESS_CODE);
  assert.equal(normalizeAccessCode('abc-defg-hi1'), null);
  assert.equal(normalizeAccessCode('abc-def-hij'), null);
  assert.equal(normalizeAccessCode('myq-xfkw-azrp'), null, 'observed 3-4-4 legacy URL remains fail-closed until its issuer is identified');
});

test('derives hidden internal credentials from access codes', () => {
  assert.equal(internalCredentialForAccessCode(ACCESS_CODE), CREDENTIAL);
  assert.equal(normalizeMeetingCredential(CREDENTIAL.toUpperCase()), CREDENTIAL);
  assert.equal(normalizeMeetingCredential('eng-sync-0123456789abcdef0123456789abcdef'), null);
});

test('generated access codes are shaped and unique against existing codes', () => {
  const generated = Array.from({ length: 200 }, () => generateAccessCode());
  assert.ok(generated.every((code) => ACCESS_CODE_SHAPE_RE.test(code)));
  assert.ok(generated.every((code) => !/[il]/.test(code)));
  assert.equal(new Set(generated).size, generated.length);
});

test('generated meeting credentials retain their access code for invite copy', () => {
  const credential = generateMeetingCode(' eng sync ');
  assert.match(credential, /^room-[0-9a-f]{32}$/);
  assert.match(accessCodeForCredential(credential) ?? '', ACCESS_CODE_SHAPE_RE);
});

test('extracts internal credentials from short-code invite inputs', () => {
  assert.equal(meetingCredentialFromInviteInput(ACCESS_CODE), CREDENTIAL);
  assert.equal(
    meetingCredentialFromInviteInput(PETAL_LIVE_CODE),
    internalCredentialForAccessCode(PETAL_LIVE_CODE)
  );
  assert.equal(meetingCredentialFromInviteInput(`https://petal.example/Design%20Review/${ACCESS_CODE}`), CREDENTIAL);
  assert.equal(meetingCredentialFromInviteInput(`https://meet.petal.live/petal-meeting/${ACCESS_CODE}`), CREDENTIAL);
  assert.equal(
    meetingCredentialFromInviteInput(`https://meet.petal.live/petal-meeting/${PETAL_LIVE_CODE}`),
    internalCredentialForAccessCode(PETAL_LIVE_CODE)
  );
  assert.equal(meetingCredentialFromInviteInput(`https://petal.example/${ACCESS_CODE}?utm=1`), CREDENTIAL);
  assert.equal(meetingCredentialFromInviteInput(`/Design%20Review/${ACCESS_CODE}`), CREDENTIAL);
  assert.equal(meetingCredentialFromInviteInput(`https://petal.example/?code=${ACCESS_CODE}`), CREDENTIAL);
  assert.equal(meetingCredentialFromInviteInput(`https://petal.example/#/join/${ACCESS_CODE}`), CREDENTIAL);
  assert.equal(meetingCredentialFromInviteInput(`petal://join/${ACCESS_CODE}`), CREDENTIAL);
});

test('does not accept old public credentials or unsupported links', () => {
  assert.equal(meetingCredentialFromInviteInput('eng-sync'), null);
  assert.equal(meetingCredentialFromInviteInput('eng-sync-0123456789abcdef0123456789abcdef'), null);
  assert.equal(meetingCredentialFromInviteInput('https://petal.example/app/name/abc-defg-hjk'), null);
  assert.equal(meetingCredentialFromInviteInput('petal://join/eng-sync'), null);
});

test('join-attempt detection covers short access code typos', () => {
  assert.equal(looksLikeMeetingCredentialInput(ACCESS_CODE), true);
  assert.equal(looksLikeMeetingCredentialInput('abcdefghjk'), true);
  assert.equal(looksLikeMeetingCredentialInput('eng-sync'), false);
  assert.equal(looksLikeMeetingCredentialInput('Design Review'), false);
});

test('builds shareable invite paths with optional display-only labels', () => {
  assert.equal(buildMeetingInvitePath('Design Review!', ACCESS_CODE), `/design-review/${ACCESS_CODE}`);
  assert.equal(buildMeetingInvitePath('', ACCESS_CODE), `/${ACCESS_CODE}`);
  assert.equal(buildMeetingInvitePath('Design Review!', 'swift-otter-lake'), null);
});
