import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { joinFailureMessage } from '../src/lib/data/joinFailure.ts';

// #356: a failed join must say WHY, in words the user can act on. The inputs
// below are the exact native rejections from the Sentry events.

test('PETAL-DESKTOP-33: a token timeout says the server could not be reached', () => {
  const message = joinFailureMessage({
    kind: 'config',
    message: 'room setup failed: backend token request timed out'
  });
  assert.match(message, /Couldn't reach Petal's server/);
  assert.match(message, /internet connection/);
  for (const cause of ['backend token connection failed', 'backend token transport failed']) {
    assert.match(joinFailureMessage({ kind: 'config', message: `room setup failed: ${cause}` }), /Couldn't reach/);
  }
});

test('PETAL-DESKTOP-35: Windows os error 10013 names the firewall/antivirus/VPN', () => {
  // Localized OS text (Italian in the real event) -- only the code is matched.
  const message = joinFailureMessage({
    kind: 'roomConnect',
    message:
      'failed to connect to LiveKit room: room connect failed: engine: signal failure: ws failure: IO error: ' +
      'Tentativo di accesso al socket con modalità non consentite dalle rispettive autorizzazioni di accesso. (os error 10013)'
  });
  assert.match(message, /firewall, antivirus or VPN blocked/);
});

test('other socket errors map to their own actionable copy on both platforms', () => {
  const at = (code: number) =>
    joinFailureMessage({ kind: 'roomConnect', message: `ws failure: IO error: x (os error ${code})` });
  assert.match(at(10061), /refused/);
  assert.match(at(61), /refused/);
  assert.match(at(10060), /timed out/);
  assert.match(at(60), /timed out/);
  for (const code of [10051, 10065, 51, 65]) assert.match(at(code), /can't be reached/);
  assert.match(at(1), /firewall/); // macOS EPERM from a network filter
  // The code match is exact: 100130 is not 10013, 1 is not 10013.
  assert.doesNotMatch(at(100130), /firewall/);
});

test('a join timeout and unknown failures still get a clear sentence', () => {
  assert.match(joinFailureMessage({ kind: 'joinTimeout', message: 'timed out joining the LiveKit room' }), /timed out/);
  assert.match(
    joinFailureMessage({ kind: 'roomConnect', message: 'failed to connect to LiveKit room: connect attempt timed out after 10s' }),
    /timed out/
  );
  assert.match(joinFailureMessage('something odd'), /could not join this room/);
  assert.match(joinFailureMessage(undefined), /could not join this room/);
});

const meetingSessionSource = readFileSync(
  fileURLToPath(new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url)),
  'utf8'
);

test('the meeting route shows the reason and offers Try again instead of auto-returning', () => {
  const attempt = meetingSessionSource.match(/async function attemptJoin\(\)[\s\S]*?\n  \}/)?.[0];
  assert.ok(attempt, 'attemptJoin should exist');
  assert.match(attempt, /disconnectDetail = joinFailureMessage\(e\);/);
  assert.doesNotMatch(attempt, /beginTerminalReturn/);
  assert.doesNotMatch(meetingSessionSource, /Could not join this room - returning you to the room list/);
  assert.match(meetingSessionSource, /label: 'Try again', onClick: \(\) => void retryJoin\(\)/);
  // Retry re-runs only the join, never re-registers the event listeners.
  const retry = meetingSessionSource.match(/async function retryJoin\(\)[\s\S]*?\n  \}/)?.[0];
  assert.ok(retry && !retry.includes('startListeners'));
});
