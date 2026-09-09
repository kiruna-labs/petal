import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { generateAccessCode, generateMeetingCode, internalCredentialForAccessCode } from '../src/lib/data/meetingCode.ts';
import { roomAccessCode } from '../src/lib/data/roomAccessCode.ts';

const roomRow = readFileSync(new URL('../src/lib/components/RoomRow.svelte', import.meta.url), 'utf8');
const liveHero = readFileSync(new URL('../src/lib/components/LiveHero.svelte', import.meta.url), 'utf8');
const mainMenu = readFileSync(new URL('../src/lib/components/MainMenu.svelte', import.meta.url), 'utf8');
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const rooms = readFileSync(new URL('../src/lib/data/roomAccessCode.ts', import.meta.url), 'utf8');

test('room access-code derivation is canonical and never falls back to an internal credential', () => {
  assert.match(rooms, /export function roomAccessCode\(/);
  assert.match(rooms, /normalizeAccessCode\(room\.accessCode\)/);
  assert.match(rooms, /accessCodeForCredential\(room\.name\)/);
  assert.doesNotMatch(rooms, /return room\.name;[\s\S]*roomAccessCode/);
});

test('room access-code derivation handles fresh, persisted, relinked, legacy, and missing records', () => {
  const fresh = generateAccessCode();
  assert.equal(roomAccessCode({ name: internalCredentialForAccessCode(fresh), accessCode: fresh }), fresh);

  const persisted = generateAccessCode();
  assert.equal(roomAccessCode({ name: 'room-a'.repeat(8), accessCode: persisted }), persisted);

  const relinked = generateMeetingCode();
  assert.match(relinked, /^room-[0-9a-f]{32}$/);
  assert.match(roomAccessCode({ name: relinked }) ?? '', /^[a-z]{3}-[a-z]{4}-[a-z]{3}$/);

  assert.equal(roomAccessCode({ name: 'room-a'.repeat(8), accessCode: 'not-a-code' }), null);
  assert.equal(roomAccessCode({ name: 'room-a'.repeat(8) }), null);
  assert.equal(roomAccessCode({ name: `room-${'a'.repeat(32)}` }), null);
});

// #123: the access code used to be an opacity-0 hover/focus disclosure. It is
// now always visible on every row. These assertions go red if the gate comes
// back (verified by re-adding `opacity: 0` to `.room-access-code`).
function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

test('room rows show the access code always, not on hover, and make the text the copy target', () => {
  // The copy-target element + Copied status are pinned in
  // mainMenuInviteLink.test.ts; this test owns the visibility and labels.
  assert.match(roomRow, /data-testid="room-access-code"/);
  assert.match(roomRow, /aria-label=\{copiedAccessCode \? `Invite link copied for \$\{name\}, room ID \$\{accessCode\}` : `Room ID \$\{accessCode\}, click to copy invite`\}/);
  assert.doesNotMatch(roomRow, /title=\{copiedAccessCode \? /, 'copy button must not emit a native tooltip title (webview tooltips off)');
  assert.doesNotMatch(roomRow, /Code: \$\{accessCode\}/);
  assert.doesNotMatch(roomRow, /room-code-copy/);
  assert.doesNotMatch(roomRow, /onCopyAccessCode/);

  const codeBlock = cssBlock(roomRow, '.room-access-code');
  assert.doesNotMatch(codeBlock, /opacity:\s*0/, '#123: the room ID must not be hidden behind a hover disclosure');
  assert.doesNotMatch(
    roomRow,
    /\.room-row-shell:hover \.room-access-code|\.room-row-shell:has\(:focus-visible\) \.room-access-code/,
    '#123: no hover/focus reveal rule may gate the room ID'
  );

  // Still never ellipsized, and the Join/star/trash hover reveal is untouched
  // (pinned separately by roomRowDisambiguation.test.ts).
  assert.match(codeBlock, /white-space: nowrap;/);
  assert.match(codeBlock, /width: max-content;/);
  assert.match(codeBlock, /flex-shrink: 0;[\s\S]*overflow: visible;/);
  assert.match(roomRow, /\.room-row-shell:hover \.join-button/);
});

test('copy feedback is success-only and the always-visible code stays keyboard-reachable', () => {
  assert.match(roomRow, /const copied = await onCopyInvite\?\.\(\);[\s\S]*if \(copied === false\) return;/);
  assert.match(roomRow, /copiedAccessCode = true;/);
  assert.match(roomRow, /\{#if copiedAccessCode\}[\s\S]*role="status" aria-live="polite"/);
  assert.match(roomRow, /\.room-access-code:focus-visible \{[\s\S]*?outline:/);
  assert.match(roomRow, /overflow: visible;/);
});

test('the live hero carries the same always-visible room ID', () => {
  assert.match(liveHero, /accessCode\?: string \| null;/);
  assert.match(liveHero, /data-testid="hero-access-code"/);
  assert.match(liveHero, /aria-label=\{copiedAccessCode \? `Invite link copied for \$\{roomName\}, room ID \$\{accessCode\}` : `Room ID \$\{accessCode\}, click to copy invite`\}/);
  const heroCodeBlock = cssBlock(liveHero, '.room-access-code');
  assert.doesNotMatch(heroCodeBlock, /opacity:\s*0/);
  assert.match(heroCodeBlock, /white-space: nowrap;/);
  assert.match(heroCodeBlock, /overflow: visible;/);
  // The hero panel must grow for the extra line instead of clipping it.
  assert.match(cssBlock(liveHero, '.hero'), /min-height: 152px;/);
  assert.doesNotMatch(cssBlock(liveHero, '.hero'), /(^|[^-])height: 152px;/);
  assert.match(mainMenu, /accessCode=\{roomAccessCodesByName\[liveRoom\.name\] \?\? null\}/);
});

test('current live room rows place meeting status before the access code', () => {
  const roomInfo = roomRow.match(/<div class="room-info">([\s\S]*?)<\/div>\s*<span class="join-label">/);
  assert.ok(roomInfo, 'live room info block should be present');

  const contents = roomInfo[1];
  assert.ok(contents.indexOf('<span class="room-name">') < contents.indexOf('{#if current}'));
  assert.ok(contents.indexOf('{#if current}') < contents.indexOf('{#if accessCode}'));
  assert.ok(contents.indexOf('{#if accessCode}') < contents.indexOf('{#if !current}'));
  assert.match(contents, /\{#if current\}[\s\S]*?<span class="room-status live-status">\{headline\}<\/span>[\s\S]*?\{\/if\}/);
});

test('non-current room occupancy status is rendered only once', () => {
  const roomInfo = roomRow.match(/<div class="room-info">([\s\S]*?)<\/div>\s*<span class="join-label">/);
  assert.ok(roomInfo, 'live room info block should be present');

  const contents = roomInfo[1];
  assert.equal((contents.match(/\{#if !current\}/g) ?? []).length, 1);
  assert.doesNotMatch(contents, /\{:else if headcount > 0\}/);
  assert.match(contents, /\{#if !current\}[\s\S]*?<span class="room-status live-status">\{headline\}<\/span>[\s\S]*?\{\/if\}/);
});

test('main menu and route pass access codes separately from safe display labels', () => {
  assert.match(mainMenu, /roomAccessCodesByName\?: Record<string, string \| null \| undefined>/);
  assert.match(mainMenu, /accessCode=\{roomAccessCodesByName\[room\] \?\? null\}/);
  assert.match(mainMenu, /normalizeMeetingCredential\(fallback\)/);
  assert.match(mainMenu, /'Petal meeting'/);
  assert.match(mainRoute, /const roomAccessCodesByName = \$derived\.by/);
  assert.match(mainRoute, /roomAccessCode\(room\)/);
  // onCopyRoomLink={copyRoomInviteLink} is pinned in mainMenuInviteLink.test.ts.
  assert.doesNotMatch(mainRoute, /copyRoomAccessCode|onCopyRoomAccessCode/);
  assert.doesNotMatch(mainMenu, /roomDisplayNamesByName\[room\]\?\.trim\(\) \|\| room/);
});
