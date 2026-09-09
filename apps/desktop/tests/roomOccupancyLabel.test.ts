// #120: the room card's "N people in the room" is the end of a chain that
// starts at LiveKit's participant count. Nothing pinned the string, or the
// fact that it renders the OCCUPANCY number rather than the roster length --
// so a wrong number could reach a user with every test still green (the
// reported bug: "7 people in the room" for a meeting of 5, because the count
// included one hidden `-gallery` bridge per desktop user).
//
// Source-text assertions, in the pattern of roomAccessCodeHover.test.ts: the
// meaning of the number is fixed on the backend (backend/test/hardening.ts),
// this file fixes what the UI does with it.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const roomRow = readFileSync(new URL('../src/lib/components/RoomRow.svelte', import.meta.url), 'utf8');
const mainMenu = readFileSync(new URL('../src/lib/components/MainMenu.svelte', import.meta.url), 'utf8');
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const roomsData = readFileSync(new URL('../src/lib/data/rooms.ts', import.meta.url), 'utf8');

test('the room card renders the headcount as a full, singular-aware sentence', () => {
  assert.match(roomRow, /'1 person in the room'/);
  assert.match(roomRow, /`\$\{headcount\} people in the room`/);
  // "1 people in the room" would be a UI-copy bug of its own: the singular
  // arm must be selected by the count, not by the presence of a roster.
  assert.match(roomRow, /headcount === 1\s*\?\s*'1 person in the room'/);
});

test('the headcount is the backend occupancy, and only when there is no local roster', () => {
  assert.match(
    roomRow,
    /const headcount = \$derived\(participants\.length === 0 && !current && \(occupancy \?\? 0\) > 0 \? occupancy! : 0\)/,
    'headcount reads `occupancy`; a room we are in or have a roster for uses the roster instead'
  );
  // The "N people in the room" arm is reachable only through headcount, so a
  // roster length can never be reported with the room-card wording.
  assert.match(roomRow, /headcount > 0\s*\?/);
  assert.match(roomRow, /participants\.length === 1\s*\?\s*'1 person is talking'/);
});

test('occupancy reaches the card unmodified: no client-side arithmetic on the count', () => {
  assert.match(mainMenu, /occupancy=\{roomOccupancyByName\[room\] \?\? null\}/);
  assert.match(mainRoute, /nextOccupancy\[room\.name\] = row\.occupancy;/);
  assert.match(roomsData, /return invoke<RoomOccupancy\[\]>\(COMMANDS\.listRoomOccupancy\)/);
  // If the count is ever wrong again, it is wrong at the source (#120 fixed
  // that in backend/lib/handlers.ts). Do not paper over it here by
  // subtracting a guessed number of hidden bridge participants: the client
  // cannot know how many there are, and web participants open none.
  assert.doesNotMatch(mainRoute, /occupancy\s*-\s*\d/);
  assert.doesNotMatch(roomRow, /occupancy\s*-\s*\d/);
});
