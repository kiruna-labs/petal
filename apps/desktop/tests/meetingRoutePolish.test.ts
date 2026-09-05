import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const meetingRoute = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);

test('meeting route does not render the remote-control active banner', () => {
  assert.doesNotMatch(meetingRoute, /remoteControlSessions/);
  assert.doesNotMatch(meetingRoute, /Disable for meeting/);
  assert.doesNotMatch(meetingRoute, /is controlling/);
});

test('meeting Share opens the picker without a stop-all toast path', () => {
  assert.match(meetingRoute, /COMMANDS\.toggleWindowPickerWindow/);
  assert.doesNotMatch(meetingRoute, /showShareToast\('Stopped sharing', 'info'\)/);
});
