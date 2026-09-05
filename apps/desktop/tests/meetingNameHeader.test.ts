import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const gallerySource = readFileSync(
  new URL('../src/lib/components/Gallery.svelte', import.meta.url),
  'utf8'
);
const mainMenuSource = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const mainRouteSource = readFileSync(
  new URL('../src/routes/main/+page.svelte', import.meta.url),
  'utf8'
);
const mainMenuActionSource = readFileSync(
  new URL('../src/lib/data/mainMenuMeetingAction.ts', import.meta.url),
  'utf8'
);
const mainMeetingActionsSource = readFileSync(
  new URL('../src/lib/data/mainMeetingActions.ts', import.meta.url),
  'utf8'
);
const meetingSessionSource = readFileSync(
  new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url),
  'utf8'
);
const meetingRouteSource = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);

test('desktop create flow keeps typed meeting name as the human display label', () => {
  // #107/#148: custom room names are display labels only. The actual joinable
  // room still uses a generated access code -> hidden 128-bit credential, same
  // as blank create, so public labels never become auth credentials.
  assert.match(mainMenuActionSource, /const displayName = trimmed;/);
  assert.match(mainMenuActionSource, /const accessCode = generateAccessCode\(\);[\s\S]*onCreateMeeting\?\.\(accessCode, displayName\)/);
  assert.match(mainMeetingActionsSource, /createRoom\(roomName, true, displayName\)/);
  assert.match(mainMeetingActionsSource, /rememberPendingRoomDisplayName\?\.\(room\.name, room\.displayName\)/);
  // #42: never fall back to the raw credential (`roomName`) as the shown
  // label — the friendly default kicks in until the real name resolves.
  assert.match(
    meetingSessionSource,
    /pendingRouteDisplayName \?\? meetingDisplayLabelFromCredential\(roomName\) \?\? 'Petal meeting'/
  );
  assert.match(meetingSessionSource, /roomDisplayLabel\(joinedRoom\)/);
});

test('desktop blank create passes an access code, not a pre-hashed credential (#107)', () => {
  assert.match(mainMenuActionSource, /const accessCode = generateAccessCode\(\)/);
  assert.match(mainMenuActionSource, /onCreateMeeting\?\.\(accessCode, null\)/);
  assert.doesNotMatch(mainMenuSource, /generateMeetingCode/);
});

test('desktop never shows the raw credential/legacy "room" label as a room name (#42)', () => {
  const roomsSource = readFileSync(
    new URL('../src/lib/data/rooms.ts', import.meta.url),
    'utf8'
  );
  assert.match(roomsSource, /'Petal meeting'/);
  assert.match(roomsSource, /isGenericRoomLabel/);
  assert.match(
    meetingRouteSource,
    /meeting\.roomLabel \|\| meetingDisplayLabelFromCredential\(roomName\) \|\| 'Petal meeting'/
  );
  assert.doesNotMatch(meetingRouteSource, /meeting\.roomLabel \|\| meetingDisplayLabelFromCredential\(roomName\) \|\| roomName/);
  // main/+page.svelte must delegate to roomDisplayLabel, not re-implement (and
  // bypass) the fallback chain by returning `room.displayName` directly.
  assert.match(mainRouteSource, /function displayLabelForRoom\(room: RoomRecord\): string \{\s*\r?\n(?:.*\r?\n)*?\s*return roomDisplayLabel\(room\);/);
});

test('desktop smart field joins strict credentials instead of creating from them', () => {
  assert.match(mainMenuSource, /const meetingInputCredential = \$derived\(meetingCredentialFromInviteInput\(meetingInputTrimmed\)\)/);
  assert.match(mainMenuSource, /meetingInputCredential \? 'Join' : meetingInputTrimmed \? 'Create' : 'Create\/Join'/);
  assert.match(mainMenuActionSource, /const credential = meetingCredentialFromInviteInput\(trimmed\)/);
  assert.match(mainMenuActionSource, /onJoinByCode\?\.\(credential, accessCode\)/);
});

test('desktop meeting-name header actions are reserved and reveal on topbar hover/focus; elapsed is always visible', () => {
  assert.match(
    gallerySource,
    /<span class="room-title">[\s\S]*class="room-name"[\s\S]*class="room-title-actions"[\s\S]*class="elapsed"/
  );
  assert.match(
    gallerySource,
    /\.room-title-actions\s*{[\s\S]*width:\s*24px;[\s\S]*opacity:\s*0;[\s\S]*pointer-events:\s*none;/
  );
  assert.match(gallerySource, /\.room-title-actions\.has-rename\s*{[\s\S]*width:\s*52px;/);
  assert.match(
    gallerySource,
    /\.topbar:hover \.room-title-actions,\s*\.room-title:has\(:focus-visible\) \.room-title-actions\s*{[\s\S]*opacity:\s*1;[\s\S]*pointer-events:\s*auto;/
  );
  // The live meeting timer is status, not chrome: always visible (UX sweep —
  // the old hover-only opacity hid it for the whole meeting).
  assert.match(
    gallerySource,
    /\.elapsed\s*{[\s\S]*font-variant-numeric:\s*tabular-nums;[\s\S]*opacity:\s*1;/
  );
  assert.doesNotMatch(gallerySource, /\.topbar:hover \.elapsed/);
  assert.doesNotMatch(gallerySource, /\.room-title:has\(:focus-visible\) \.elapsed/);
  assert.match(gallerySource, /color:\s*var\(--text-faint\);/);
  assert.match(
    gallerySource,
    /\.room-title-actions\s*{[\s\S]*transition:\s*opacity var\(--motion-fast\) var\(--ease-standard\);/
  );
  assert.doesNotMatch(gallerySource, /\.room-title:hover \.room-title-actions/);
  assert.doesNotMatch(gallerySource, /\.room-title:hover \.elapsed/);
  assert.doesNotMatch(gallerySource, /\.topbar-left:(hover|focus-within) \.room-title-actions/);
});

test('desktop room title icon buttons are matching square controls', () => {
  assert.match(
    gallerySource,
    /\.room-title-action\s*{[\s\S]*width:\s*24px;[\s\S]*height:\s*24px;[\s\S]*border-radius:\s*var\(--radius-chip\);/
  );
  assert.doesNotMatch(gallerySource, /\.room-rename-button\s*{/);
});

test('desktop meeting-name header keeps copy and real rename controls accessible', () => {
  assert.match(gallerySource, /aria-label=\{inviteAriaLabel\}/);
  assert.doesNotMatch(gallerySource, /title=\{inviteTooltip\}/, 'copy button must not emit a native tooltip title');
  assert.match(gallerySource, /onclick=\{copyRoomInvite\}/);
  assert.match(gallerySource, /aria-label="Rename room"/);
  assert.match(gallerySource, /onclick=\{beginRoomRename\}/);
  assert.match(gallerySource, /roomNameInput\?\.focus\(\)/);
});
