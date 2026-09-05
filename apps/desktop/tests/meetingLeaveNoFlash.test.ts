import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

// Regression guard for the UX bug: leaving the meeting (pill Leave circle)
// flashed the yellow 'Disconnected' terminal card for up to 900ms. Cause:
// `leave_room` emits `room-left` (session/room.rs), and the room-left
// listener mapped it to beginTerminalReturn — correct for an externally
// ended meeting, wrong for the user's own Leave, which already navigates
// via handleLeave. Fix: a self-leave guard set BEFORE leaveRoom() that the
// listener checks first.
const meetingSessionSource = readFileSync(
  fileURLToPath(new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url)),
  'utf8'
);

test('handleLeave sets the self-leave guard before leaveRoom', () => {
  const handleLeave = meetingSessionSource.match(/async function handleLeave\(\)[\s\S]*?\n  \}/)?.[0];
  assert.ok(handleLeave, 'handleLeave should exist');
  const guardIndex = handleLeave.indexOf('selfLeaveRequested = true');
  const leaveRoomIndex = handleLeave.indexOf('await leaveRoom()');
  assert.ok(guardIndex !== -1, 'handleLeave must set selfLeaveRequested');
  assert.ok(leaveRoomIndex !== -1, 'handleLeave must call leaveRoom');
  // The guard must be set BEFORE leaveRoom(): the Rust side emits room-left
  // while the command runs, so setting it after would still flash the card.
  assert.ok(
    guardIndex < leaveRoomIndex,
    'selfLeaveRequested must be set before await leaveRoom()'
  );
});

test('room-left listener skips the terminal card for a self-initiated leave', () => {
  const listener = meetingSessionSource.match(/unlistenRoomLeft = await listen[\s\S]*?\n\s*\}\);/)?.[0];
  assert.ok(listener, 'room-left listener should exist');
  assert.match(
    listener,
    /if \(selfLeaveRequested\) return;/,
    'listener must early-return when the leave was self-initiated'
  );
  // The external-leave path (meeting ended by the host / menubar Leave)
  // must still show the terminal card — only the self-leave case is silent.
  assert.match(
    listener,
    /beginTerminalReturn\('Meeting ended - returning you to the room list\.'\);/,
    'external leaves must still render the terminal return card'
  );
});
