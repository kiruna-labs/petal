import assert from 'node:assert/strict';
import test from 'node:test';
import { meetingActionErrorMessage } from '../src/meetingActionError.ts';

test('web meeting action errors map network failures to actionable copy', () => {
  assert.equal(
    meetingActionErrorMessage(new Error('network timeout while fetching token'), 'join'),
    'Could not join the meeting. Petal could not reach the meeting server. Check your connection and try again.'
  );
});

test('web meeting action errors avoid raw object and arbitrary backend detail', () => {
  assert.equal(
    meetingActionErrorMessage({ message: 'exploded' }, 'create'),
    'Could not create the meeting. Check your connection and try again.'
  );
  assert.equal(
    meetingActionErrorMessage(new Error('opaque internal details'), 'leave'),
    'Could not leave the meeting. Check your connection and try again.'
  );
});

test('web meeting action errors include invite recovery copy', () => {
  assert.equal(
    meetingActionErrorMessage('malformed invite link', 'join'),
    'Could not join the meeting. Check the invite link or meeting code and try again.'
  );
});
