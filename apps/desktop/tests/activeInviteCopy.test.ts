import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  findRoomRecord,
  inviteCopyAriaLabel,
  inviteCopyTooltip,
  inviteLinkForAccessCode,
  meetingInviteAccessCode,
  publicInviteAccessCode
} from '../src/lib/data/inviteLinks.ts';
import { internalCredentialForAccessCode } from '../src/lib/data/meetingCode.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const credential = internalCredentialForAccessCode(ACCESS_CODE);
const gallerySource = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');
const chromeSource = readFileSync(new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url), 'utf8');
const routeSource = readFileSync(new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url), 'utf8');
const sessionSource = readFileSync(new URL('../src/lib/meeting/meetingSession.svelte.ts', import.meta.url), 'utf8');
const popoverSource = readFileSync(new URL('../src/routes/menubar-popover/+page.svelte', import.meta.url), 'utf8');

// A room the NATIVE side created or stored (rooms.json): this webview never
// generated or parsed its code, so the in-memory credential map is empty for it.
const NATIVE_CODE = 'hfs-tgdu-kmn';
const nativeCredential = `room-${'6b'.repeat(16)}`;
const savedRoom = {
  name: nativeCredential,
  slug: nativeCredential,
  accessCode: NATIVE_CODE,
  displayName: 'Max testing'
};

test('active invite copy labels disclose only the public access code', () => {
  assert.equal(publicInviteAccessCode(credential), ACCESS_CODE);
  assert.equal(inviteCopyTooltip(credential), `Room ID: ${ACCESS_CODE} (click to copy invite)`);
  assert.equal(inviteCopyAriaLabel(credential), `Room ID ${ACCESS_CODE}, click to copy invite`);
  assert.equal(inviteCopyTooltip('room-not-a-public-code'), 'Copy invite link');
  assert.equal(inviteCopyAriaLabel(null), 'Copy invite link');
});

test('desktop active-meeting invite controls receive the public-code labels on every density', () => {
  assert.match(routeSource, /const inviteAccessCode = \$derived\(meeting\.inviteAccessCode\)/);
  assert.match(routeSource, /inviteLinkForAccessCode\([\s\S]*meeting\.inviteAccessCode\s*\)/);
  assert.match(routeSource, /const inviteAriaLabel = \$derived\(inviteCopyAriaLabel\(inviteAccessCode\)\)/);
  assert.match(routeSource, /<MeetingChrome[\s\S]*\{inviteAriaLabel\}[\s\S]*\{inviteTooltip\}/);

  assert.match(gallerySource, /aria-label=\{inviteAriaLabel\}/);
  assert.doesNotMatch(gallerySource, /title=\{inviteTooltip\}/, 'copy button must not emit a native tooltip title');
  assert.match(gallerySource, /icon="invite"[\s\S]*label=\{inviteAriaLabel\}/);
  assert.doesNotMatch(gallerySource, /tooltip=\{inviteTooltip\}/, 'ControlButton no longer receives a native-tooltip title prop');
  assert.match(gallerySource, /class="control-tooltip invite-control-tooltip"[^>]*>\{inviteTooltip\}/);

  assert.match(chromeSource, /case 'invite':[\s\S]*return inviteAriaLabel;/);
  assert.match(chromeSource, /<Gallery[\s\S]*\{inviteAriaLabel\}[\s\S]*\{inviteTooltip\}/);
  assert.match(chromeSource, /class:invite-control-tooltip=\{icon === 'invite'\}>\{tooltipFor\(icon\)\}/);
  assert.doesNotMatch(chromeSource, /tooltip=\{tooltipFor\(icon\)\}/, 'ControlButton no longer receives a native-tooltip title prop');
});

test('desktop invite tooltip stays readable and shifts inside viewport gutters', () => {
  assert.match(gallerySource, /\.invite-control-tooltip\s*\{[\s\S]*width:\s*min\(220px,\s*calc\(100vw\s*-\s*24px\)\);[\s\S]*box-sizing:\s*border-box;[\s\S]*white-space:\s*normal;[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*text-wrap:\s*pretty;/);
  assert.match(gallerySource, /const INVITE_TOOLTIP_GUTTER_PX = 12;/);
  assert.match(gallerySource, /const unshiftedLeft = rect\.left - inviteTooltipShift;[\s\S]*const unshiftedRight = rect\.right - inviteTooltipShift;/);
  assert.match(gallerySource, /inviteTooltipShift = unshiftedLeft < INVITE_TOOLTIP_GUTTER_PX[\s\S]*unshiftedRight > window\.innerWidth - INVITE_TOOLTIP_GUTTER_PX/);
  assert.match(gallerySource, /onmouseenter=\{keepInviteTooltipInViewport\} onfocusin=\{keepInviteTooltipInViewport\}/);
  assert.match(gallerySource, /<svelte:window onresize=\{keepInviteTooltipInViewport\} \/>/);
  assert.match(gallerySource, /\.control-cell:hover \.invite-control-tooltip,[\s\S]*transform:\s*translate\(calc\(-50% \+ var\(--invite-tooltip-shift, 0px\)\), 0\);/);
  assert.match(chromeSource, /\.more-item \.invite-control-tooltip\s*\{[\s\S]*white-space:\s*normal;[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*text-wrap:\s*pretty;/);
});

test('a joining meeting (joinedRoom still null) invites from the saved record, not the empty credential map', () => {
  // 2026-09-29: join_room held for 44s on a microphone timeout (#787); the
  // Invite control read joinedRoom (null) then the credential map (empty for a
  // native-created room) and said the access code needed repair.
  assert.equal(publicInviteAccessCode(nativeCredential), null, 'precondition: the map has never seen this room');
  assert.equal(meetingInviteAccessCode(null, null, nativeCredential), null);

  const code = meetingInviteAccessCode(null, findRoomRecord([savedRoom], nativeCredential), nativeCredential);
  assert.equal(code, NATIVE_CODE);
  assert.equal(inviteLinkForAccessCode('Max testing', code), `https://meet.petal.live/max-testing/${NATIVE_CODE}`);
});

test('meeting invite code: joined record wins, and the credential never stands in for a code (#42)', () => {
  assert.equal(meetingInviteAccessCode({ accessCode: ACCESS_CODE }, savedRoom, nativeCredential), ACCESS_CODE);
  assert.equal(meetingInviteAccessCode({ accessCode: null }, savedRoom, nativeCredential), NATIVE_CODE);
  assert.equal(meetingInviteAccessCode(null, null, credential), ACCESS_CODE, 'codes this webview parsed still resolve');
  assert.equal(meetingInviteAccessCode(null, { accessCode: nativeCredential }, nativeCredential), null);
  assert.equal(meetingInviteAccessCode(null, null, ACCESS_CODE.toUpperCase()), ACCESS_CODE);
});

test('findRoomRecord matches the durable name or slug and nothing else', () => {
  const other = { ...savedRoom, name: `room-${'0'.repeat(32)}`, slug: 'other-slug' };
  assert.equal(findRoomRecord([other, savedRoom], nativeCredential), savedRoom);
  assert.equal(findRoomRecord([other, savedRoom], 'other-slug'), other);
  assert.equal(findRoomRecord([other, savedRoom], NATIVE_CODE), null);
  assert.equal(findRoomRecord([other, savedRoom], null), null);
});

test('the meeting session loads the saved record at join without awaiting it', () => {
  assert.match(sessionSource, /async function join\(\): Promise<boolean> \{[\s\S]*?void loadSavedRoom\(\);[\s\S]*?await startListeners\(\);/);
  assert.match(sessionSource, /findRoomRecord\(await listRooms\(\), roomName\)/);
  assert.match(sessionSource, /meetingInviteAccessCode\(joinedRoom, savedRoom, roomName\)/);
  assert.match(popoverSource, /let room = findRoomRecord\(rooms, roomName\);[\s\S]*?rooms = await listRooms\(\);/);
});
