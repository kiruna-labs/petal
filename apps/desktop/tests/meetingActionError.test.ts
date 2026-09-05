import assert from 'node:assert/strict';
import test from 'node:test';
import { meetingActionErrorMessage } from '../src/lib/data/meetingActionError.ts';

test('meeting action errors map network failures to actionable copy', () => {
  assert.equal(
    meetingActionErrorMessage(new Error('Failed to fetch token'), 'create'),
    'Could not create the meeting. Petal could not reach the meeting server. Check your connection and try again.'
  );
});

test('meeting action errors do not expose raw object or arbitrary backend detail', () => {
  assert.equal(
    meetingActionErrorMessage({ message: 'exploded' }, 'join'),
    'Could not join the meeting. Check your connection and try again.'
  );
  assert.equal(
    meetingActionErrorMessage(new Error('opaque internal details'), 'leave'),
    'Could not leave the meeting. Check your connection and try again.'
  );
});

test('meeting action errors include invite and route-specific recovery copy', () => {
  assert.equal(
    meetingActionErrorMessage('invalid access code', 'join'),
    'Could not join the meeting. Check the invite link or meeting code and try again.'
  );
  assert.equal(
    meetingActionErrorMessage('goto failed', 'open'),
    'Meeting was created, but Petal could not open it. Try selecting it from Your Rooms.'
  );
});
