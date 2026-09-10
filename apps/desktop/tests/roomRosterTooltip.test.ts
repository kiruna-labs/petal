// #122: hovering (or keyboard-focusing) a live room row shows WHO is in it.
//
// Three separate things are pinned here, and they fail for different reasons:
//   1. the shared name formatter (shared/logic/participantNames.ts), which
//      both this app and the web client consume,
//   2. the tooltip placement arithmetic, and
//   3. the wiring -- the source-level facts a rendered test cannot assert
//      cheaply, above all that the status endpoint's names do NOT reach
//      `presenceByRoom`, because that map drives hero promotion and the live
//      sort. The rendered proof of the pixels lives in
//      roomRosterTooltipRendered.test.ts.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  looksLikeTechnicalIdentity,
  participantNameLabel,
  participantNamesSummary,
  PARTICIPANT_SUMMARY_MAX,
  UNNAMED_PARTICIPANT
} from '../src/lib/data/participantNames.ts';
import { placeRowTooltip, TOOLTIP_GAP, TOOLTIP_MARGIN } from '../src/lib/data/rowTooltipPlacement.ts';

const roomRow = readFileSync(new URL('../src/lib/components/RoomRow.svelte', import.meta.url), 'utf8');
const mainMenu = readFileSync(new URL('../src/lib/components/MainMenu.svelte', import.meta.url), 'utf8');
const mainRoute = readFileSync(new URL('../src/routes/main/+page.svelte', import.meta.url), 'utf8');
const sharedNames = readFileSync(
  new URL('../../../shared/logic/participantNames.ts', import.meta.url),
  'utf8'
);
const webTiles = readFileSync(new URL('../../../web-harness/src/tiles.ts', import.meta.url), 'utf8');

function cssBlock(source: string, selector: string): string {
  const marker = `${selector} {`;
  const start = source.indexOf(marker);
  assert.notEqual(start, -1, `missing CSS block for ${selector}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('}', bodyStart);
  assert.notEqual(end, -1, `unterminated CSS block for ${selector}`);
  return source.slice(bodyStart, end);
}

test('a name that is really a machine identity is never rendered as a name', () => {
  // The three identity shapes the backend mints and accepts. A LiveKit
  // participant with no displayName carries its identity in `name`.
  assert.equal(looksLikeTechnicalIdentity('11111111-1111-4111-8111-111111111111'), true);
  assert.equal(looksLikeTechnicalIdentity('web-22222222-2222-4222-8222-222222222222'), true);
  assert.equal(looksLikeTechnicalIdentity('8535e993a1b76ed8a9ee59b265f53dfc'), true);
  assert.equal(looksLikeTechnicalIdentity('Ada'), false);
  assert.equal(looksLikeTechnicalIdentity('Bruno Fernandes'), false);

  assert.equal(participantNameLabel(''), UNNAMED_PARTICIPANT);
  assert.equal(participantNameLabel('   '), UNNAMED_PARTICIPANT);
  assert.equal(participantNameLabel(undefined), UNNAMED_PARTICIPANT);
  assert.equal(participantNameLabel('11111111-1111-4111-8111-111111111111'), UNNAMED_PARTICIPANT);
  assert.equal(participantNameLabel('web-22222222-2222-4222-8222-222222222222'), UNNAMED_PARTICIPANT);
  assert.equal(participantNameLabel('  Ada  '), 'Ada');
});

test('participantNamesSummary spells out a few names and then counts the rest', () => {
  assert.equal(participantNamesSummary([]), '');
  assert.equal(participantNamesSummary(['Ada']), 'Ada');
  assert.equal(participantNamesSummary(['Ada', 'Bruno']), 'Ada and Bruno');
  assert.equal(participantNamesSummary(['Ada', 'Bruno', 'Wren']), 'Ada, Bruno and Wren');
  assert.equal(participantNamesSummary(['Ada', 'Bruno', 'Wren', 'Yusuf']), 'Ada, Bruno, Wren and 1 more');

  // The backend caps its list at 32; the tooltip spells out three of them.
  const many = Array.from({ length: 33 }, (_unused, i) => `Person ${i}`);
  assert.equal(participantNamesSummary(many), 'Person 0, Person 1, Person 2 and 30 more');
  assert.equal(PARTICIPANT_SUMMARY_MAX, 3);

  // A blank and an identity-shaped name still occupy a slot -- they are real
  // people -- they just read as "Someone".
  assert.equal(
    participantNamesSummary(['', 'web-22222222-2222-4222-8222-222222222222', 'Ada']),
    'Someone, Someone and Ada'
  );
});

test('the technical-identity check lives in shared/, consumed by BOTH clients', () => {
  assert.match(sharedNames, /export function looksLikeTechnicalIdentity/);
  assert.match(
    webTiles,
    /export \{ looksLikeTechnicalIdentity \} from '@petal\/shared\/logic\/participantNames';/,
    'the web client must re-export the shared definition, not keep its own copy'
  );
  assert.doesNotMatch(
    webTiles,
    /export function looksLikeTechnicalIdentity/,
    'a second definition would let the two surfaces drift'
  );
});

test('the tooltip is placed below the row, flipped when there is no room, and clamped inside the window', () => {
  const size = { width: 200, height: 40 };
  const viewport = { width: 400, height: 600 };

  const below = placeRowTooltip({ top: 100, bottom: 140, left: 12, right: 388 }, size, viewport);
  assert.equal(below.placement, 'below');
  assert.equal(below.top, 140 + TOOLTIP_GAP);
  assert.equal(below.left, 12);

  // A row at the very bottom of a scrolled list: below would run off screen.
  const flipped = placeRowTooltip({ top: 540, bottom: 580, left: 12, right: 388 }, size, viewport);
  assert.equal(flipped.placement, 'above');
  assert.equal(flipped.top, 540 - TOOLTIP_GAP - size.height);
  assert.ok(flipped.top >= TOOLTIP_MARGIN);

  // Never off the right edge, never off the left.
  const rightEdge = placeRowTooltip({ top: 10, bottom: 50, left: 380, right: 396 }, size, viewport);
  assert.equal(rightEdge.left, viewport.width - TOOLTIP_MARGIN - size.width);
  const leftEdge = placeRowTooltip({ top: 10, bottom: 50, left: -30, right: 100 }, size, viewport);
  assert.equal(leftEdge.left, TOOLTIP_MARGIN);

  // A tooltip taller than the window still starts on screen rather than
  // being pushed above the top edge.
  const tall = placeRowTooltip({ top: 300, bottom: 340, left: 12, right: 388 }, { width: 200, height: 900 }, viewport);
  assert.ok(tall.top >= 0);
});

test('the row tooltip is a styled element, never a native title, revealed on hover and focus', () => {
  assert.match(roomRow, /data-testid="room-roster-tooltip"/);
  assert.match(roomRow, /role="tooltip"/);
  // `title=` is stripped at runtime on this surface (suppressNativeTooltips).
  const tooltipMarkup = roomRow
    .slice(
      roomRow.indexOf('{#snippet rosterTooltip()}'),
      roomRow.indexOf('{/snippet}', roomRow.indexOf('{#snippet rosterTooltip()}'))
    )
    .replace(/<!--[\s\S]*?-->/g, '');
  assert.ok(tooltipMarkup.length > 0, 'the roster tooltip snippet is missing');
  assert.doesNotMatch(tooltipMarkup, /\btitle=/, 'the tooltip must not rely on a native title attribute');

  assert.match(
    roomRow,
    /\.room-row-shell:hover \.room-roster-tooltip,\s*\.room-row-shell:focus-visible \.room-roster-tooltip,\s*\.room-row-shell:has\(:focus-visible\) \.room-roster-tooltip \{\s*opacity: 1;/,
    'reveal must key off the row shell itself (tabindex=0) AND its inner controls'
  );
  assert.match(
    roomRow,
    /\.room-row-shell\.clickable:active \.room-roster-tooltip \{\s*opacity: 0;/,
    'the press transform makes the row the containing block; the tooltip must hide'
  );

  const block = cssBlock(roomRow, '.room-roster-tooltip');
  assert.match(block, /position: fixed/, 'an absolute tooltip is clipped by the scrolling room list');
  assert.match(block, /var\(--motion-tooltip-delay\)/);
  assert.doesNotMatch(block, /white-space:\s*nowrap/, 'the names must wrap, never truncate');
  assert.doesNotMatch(block, /text-overflow/, 'no ellipsis: UI text must never truncate');
});

test('the row exposes the same names to assistive tech as it shows on hover', () => {
  assert.match(roomRow, /const rosterSummary = \$derived\(participantNamesSummary\(roster\)\)/);
  assert.match(roomRow, /const rosterLabel = \$derived\(rosterSummary \? `\$\{rosterSummary\} in this room`/);
  assert.match(roomRow, /aria-label=\{rowLabel\}/);
  assert.match(roomRow, /aria-describedby=\{rosterSummary \? tooltipId : undefined\}/);
});

test('roster names are plumbed as their own prop, never through participants', () => {
  assert.match(roomRow, /roster\?: string\[\];/);
  assert.match(mainMenu, /roomRosterByName\?: Record<string, string\[\]>;/);
  assert.match(mainMenu, /roster=\{roomRosterByName\[room\] \?\? \[\]\}/);
  assert.match(mainRoute, /roomRosterByName=\{roomRosterByName\}/);
  // The avatar branch still keys off `participants`, which the status lookup
  // no longer feeds -- so a live row keeps its dot and its position.
  assert.match(roomRow, /\{#if participants\.length\}/);
});

// The trap this issue was filed with: `presenceByRoom` drives
// `promotedLiveRoom` (hero promotion), `roomListPriority` (the live sort) and
// RoomRow's avatar branch. It was safe to write status participants into it
// only because they were ALWAYS empty. The moment the backend returns names,
// that same line would pull the live room out of the list into the hero and
// re-sort the list. Restoring the old line is the mutation that turns the
// rendered hero regression test red.
test('the status lookup never feeds presenceByRoom -- hero promotion and the sort are untouched', () => {
  assert.doesNotMatch(
    mainRoute,
    /nextPresence\[room\.name\] = visibleParticipants\(row\.participants/,
    '#122: routing status names into presence would silently promote and re-sort rooms'
  );
  assert.match(mainRoute, /let statusRosterByRoom = \$state<Record<string, string\[\]>>\(\{\}\)/);
  assert.match(mainRoute, /nextRoster\[room\.name\] = row\.participants\.map\(\(p\) => p\.name\)/);
  assert.match(
    mainRoute,
    /if \(row\.participants\) \{/,
    'an omitted roster must leave the key out, not write an empty array'
  );
  // `promotedLiveRoom` and `roomListPriority` still read presence only.
  assert.match(mainRoute, /const name = orderedRoomNames\.find\(\(roomName\) => \(presenceByRoom\[roomName\]\?\.length \?\? 0\) > 0\)/);
  assert.match(mainRoute, /return isRoomLive\(name\) \? 1 : 0;/);
});

test('the joined room and every other row use the same roster prop', () => {
  assert.match(
    mainRoute,
    /const merged: Record<string, string\[\]> = \{ \.\.\.statusRosterByRoom \};[\s\S]*presenceByRoom[\s\S]*participants\.map\(\(p\) => p\.name\)/,
    'the room you are in must fill the same tooltip from its live presence roster'
  );
});
