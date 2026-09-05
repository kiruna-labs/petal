import assert from 'node:assert/strict';
import test from 'node:test';

import { normalizeJoinRoomName, parseJoinInput } from '../src/lib/data/joinInput.ts';
import { generateMeetingCode, slugifyMeetingCode } from '../src/lib/data/meetingCode.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = 'room-8535e993a1b76ed8a9ee59b265f53dfc';

test('meeting-code normalization mirrors room slug identity', () => {
  assert.equal(slugifyMeetingCode('Webtest'), 'webtest');
  assert.equal(slugifyMeetingCode('Design Review!'), 'design-review');
  assert.equal(slugifyMeetingCode('---'), 'room');
});

test('normalizeJoinRoomName requires an internal credential', () => {
  assert.equal(normalizeJoinRoomName(CREDENTIAL.toUpperCase()), CREDENTIAL);
  assert.equal(normalizeJoinRoomName(ACCESS_CODE), null);
  assert.equal(normalizeJoinRoomName('eng-sync'), null);
});

test('parseJoinInput returns access codes for bare codes and invite links', () => {
  assert.deepEqual(parseJoinInput(` ${ACCESS_CODE.toUpperCase()} `), { ok: true, room: ACCESS_CODE });
  assert.deepEqual(parseJoinInput(`petal://join/${ACCESS_CODE}`), { ok: true, room: ACCESS_CODE });
  assert.deepEqual(parseJoinInput(`https://petal.local/Design%20Review/${ACCESS_CODE}`), {
    ok: true,
    room: ACCESS_CODE
  });
});

test('generated meeting credentials are internal only', () => {
  const generated = generateMeetingCode();
  assert.match(generated, /^room-[0-9a-f]{32}$/);
});

test('parseJoinInput rejects label-only input and old public credentials', () => {
  assert.deepEqual(parseJoinInput('eng-sync'), {
    ok: false,
    error: 'Paste a full invite link or meeting code.'
  });
  assert.deepEqual(parseJoinInput('eng-sync-0123456789abcdef0123456789abcdef'), {
    ok: false,
    error: 'Paste a full invite link or meeting code.'
  });
});
