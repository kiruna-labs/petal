import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  inviteLinkCopiedToastMessage,
  inviteLinkForAccessCode,
  inviteLinkForRoom,
  INVITE_LINK_COPIED_LABEL,
  INVITE_ORIGIN
} from '../src/lib/data/inviteLinks.ts';

const roomRow = readFileSync(
  new URL('../src/lib/components/RoomRow.svelte', import.meta.url),
  'utf8'
);
const mainMenu = readFileSync(
  new URL('../src/lib/components/MainMenu.svelte', import.meta.url),
  'utf8'
);
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const mainMenuAction = readFileSync(
  new URL('../src/lib/data/mainMenuMeetingAction.ts', import.meta.url),
  'utf8'
);
const mainMeetingActions = readFileSync(
  new URL('../src/lib/data/mainMeetingActions.ts', import.meta.url),
  'utf8'
);
const meetingActionError = readFileSync(
  new URL('../src/lib/data/meetingActionError.ts', import.meta.url),
  'utf8'
);
const meetingRoute = readFileSync(
  new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url),
  'utf8'
);
const menubarRoute = readFileSync(
  new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url),
  'utf8'
);

test('shared invite-link helper builds the deployed join URL', () => {
  assert.equal(INVITE_ORIGIN, 'https://meet.petal.live');
  assert.equal(
    inviteLinkForAccessCode('Design Review!', 'abc-defg-hjk'),
    'https://meet.petal.live/design-review/abc-defg-hjk'
  );
  assert.equal(
    inviteLinkForRoom({
      id: '1',
      name: 'rapid-noble-raven-9d08320988f3169ff261f531523d1295',
      accessCode: 'abc-defg-hjk',
      displayName: 'Petal meeting',
      slug: 'rapid-noble-raven',
      createdAtMs: 0,
      open: true
    }),
    'https://meet.petal.live/petal-meeting/abc-defg-hjk'
  );
});

test('invite-link helper does not invent a URL without an access code', () => {
  assert.equal(inviteLinkForAccessCode('Design Review!', null), null);
});

test('shared invite-link copied toast message keeps label and URL on separate lines', () => {
  const link = 'https://meet.petal.live/design-review/abc-defg-hjk';

  assert.equal(INVITE_LINK_COPIED_LABEL, 'Invite link copied to clipboard:');
  assert.equal(inviteLinkCopiedToastMessage(link), `${INVITE_LINK_COPIED_LABEL}\n${link}`);
});

test('main menu room rows expose the canonical access-code copy target and feedback', () => {
  assert.match(roomRow, /onCopyInvite\?: \(\) => boolean \| void \| Promise<boolean \| void>/);
  assert.match(roomRow, /class="room-access-code"/);
  assert.doesNotMatch(roomRow, /Room ID: \$\{accessCode\} \(click to copy invite\)/, 'native tooltip copy must not be emitted');
  assert.match(roomRow, /Room ID \$\{accessCode\}, click to copy invite/);
  assert.match(roomRow, /class="room-access-code-status" role="status" aria-live="polite">Copied/);
  assert.doesNotMatch(roomRow, /copy-button|copiedInvite|copyInvite|copyTimer/);
  assert.match(mainMenu, /onCopyRoomLink\?: \(name: string\) => boolean \| void \| Promise<boolean \| void>/);
  assert.match(mainMenu, /onCopyInvite=\{onCopyRoomLink \? \(\) => onCopyRoomLink\(room\) : undefined\}/);
  assert.doesNotMatch(mainMenu, /onCopyRoomAccessCode/);
});

test('main route copies each saved room invite with native clipboard fallback', () => {
  assert.match(mainRoute, /import \{ writeText \} from '@tauri-apps\/plugin-clipboard-manager'/);
  assert.match(mainRoute, /import \{ inviteLinkForRoom \} from '\$lib\/data\/inviteLinks'/);
  assert.match(mainRoute, /const link = inviteLinkForRoom\(room, displayLabelForRoom\(room\)\)/);
  assert.match(mainRoute, /async function copyRoomInviteLink\(name: string\): Promise<boolean>/);
  assert.match(mainRoute, /await writeText\(link\)/);
  assert.match(mainRoute, /await navigator\.clipboard\.writeText\(link\)/);
  assert.match(mainRoute, /return true;/);
  assert.match(mainRoute, /return false;/);
  assert.match(mainRoute, /onCopyRoomLink=\{copyRoomInviteLink\}/);
});

test('main menu create and join action failures surface a visible inline error', () => {
  assert.match(mainMenu, /meetingActionError\?: string \| null/);
  assert.match(mainMenu, /const visibleJoinError = \$derived\(joinError \?\? meetingActionError\)/);
  assert.match(mainMenu, /let meetingSubmitting = \$state\(false\)/);
  assert.match(mainMenu, /await submitMainMenuMeetingAction\(meetingInput/);
  assert.match(mainMenu, /meetingInput = result\.nextInput/);
  assert.match(mainMenu, /joinError = result\.error/);
  assert.match(mainMenu, /onClearMeetingActionError\?: \(\) => void/);
  assert.match(mainMenu, /oninput=\{clearJoinError\}/);
  assert.match(mainMenu, /<p class="join-error" role="alert">\{visibleJoinError\}<\/p>/);
  assert.match(mainMenuAction, /meetingActionErrorMessage\(error, action\)/);
  assert.match(mainMenuAction, /INVALID_MEETING_INPUT_ERROR = 'Paste a full invite link or meeting code.'/);

  assert.match(mainRoute, /let meetingActionError = \$state<string \| null>\(null\)/);
  assert.match(mainRoute, /createMainMeetingActions\(\{/);
  assert.match(mainMeetingActions, /room = await createRoom\(accessCode \?\? roomName, true\)/);
  assert.match(mainMeetingActions, /room = await createRoom\(roomName, true, displayName\)/);
  assert.match(mainMeetingActions, /async function navigateToMeeting\(room: RoomRecord, source: MeetingNavigationSource\)/);
  assert.match(meetingActionError, /Meeting was created, but Petal could not open it/);
  assert.match(mainMeetingActions, /setMeetingActionError\(meetingActionErrorMessage\(e, 'join'\)\)/);
  assert.match(mainMeetingActions, /setMeetingActionError\(meetingActionErrorMessage\(e, 'create'\)\)/);
  assert.match(mainMeetingActions, /setMeetingActionError\(meetingActionErrorMessage\(e, 'open'\)\)/);
  assert.match(meetingActionError, /Petal could not reach the meeting server/);
  assert.match(mainRoute, /\{meetingActionError\}/);
  assert.match(mainRoute, /onClearMeetingActionError=\{\(\) => \(meetingActionError = null\)\}/);
});

test('main route keeps the joined room in YOUR ROOMS as the current row', () => {
  assert.match(mainRoute, /currentRoom=\{joinedRoomName\}/);
  assert.match(mainRoute, /\.sort\(\(a, b\) => roomListPriority\(b\.name\) - roomListPriority\(a\.name\)\)/);
  assert.match(mainRoute, /if \(joinedRoomName && roomKey\(name\) === roomKey\(joinedRoomName\)\) return 2/);
  assert.match(mainRoute, /orderedRoomNames\.filter\(\(name\) => !promotedLiveRoom \|\| name !== promotedLiveRoom\.name\)/);
  assert.doesNotMatch(mainRoute, /name !== joinedRoomName/);
  assert.doesNotMatch(mainRoute, /<section class="in-meeting"/);
  assert.match(mainMenu, /currentRoom\?: string \| null/);
  assert.match(mainMenu, /\{@const rowIsCurrent = currentRoomLower === room\.trim\(\)\.toLowerCase\(\)\}/);
  assert.match(mainMenu, /current=\{rowIsCurrent\}/);
  assert.match(roomRow, /current\?: boolean/);
  assert.match(roomRow, /current\s*\?\s*'In meeting'/);
  assert.match(roomRow, /current \? 'Return' : 'Join now'/);
});

test('main menu hides forget control for the currently joined room row', () => {
  // rowIsCurrent + current={rowIsCurrent} wiring is pinned by the
  // "keeps the joined room in YOUR ROOMS as the current row" test above.
  assert.match(mainMenu, /onRemove=\{onRemoveRoom && !rowIsCurrent \? \(\) => onRemoveRoom\(room\) : undefined\}/);
  assert.doesNotMatch(mainMenu, /onRemove=\{onRemoveRoom \? \(\) => onRemoveRoom\(room\) : undefined\}/);
});

test('meeting and menubar copy paths share invite-link construction', () => {
  assert.match(meetingRoute, /inviteLinkCopiedToastMessage,/);
  assert.match(meetingRoute, /inviteLinkForAccessCode/);
  assert.match(meetingRoute, /return inviteLinkForAccessCode\(/);
  assert.match(meetingRoute, /inviteToast\.show\(inviteLinkCopiedToastMessage\(link\)\)/);
  assert.doesNotMatch(meetingRoute, /const INVITE_ORIGIN/);
  assert.match(menubarRoute, /import \{ inviteLinkCopiedToastMessage, inviteLinkForRoom \} from '\$lib\/data\/inviteLinks'/);
  assert.match(menubarRoute, /room \? inviteLinkForRoom\(room, label\) : null/);
  assert.match(menubarRoute, /copiedLink = ok \? inviteLinkCopiedToastMessage\(link\) : ''/);
  assert.match(menubarRoute, /overflow-wrap: anywhere;/);
  assert.match(menubarRoute, /white-space: pre-line;/);
});
