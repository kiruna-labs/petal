import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  inviteCopyAriaLabel,
  inviteCopyTooltip,
  publicInviteAccessCode
} from '../src/lib/data/inviteLinks.ts';
import { internalCredentialForAccessCode } from '../src/lib/data/meetingCode.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const credential = internalCredentialForAccessCode(ACCESS_CODE);
const gallerySource = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');
const chromeSource = readFileSync(new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url), 'utf8');
const routeSource = readFileSync(new URL('../src/routes/meeting/[room]/+page.svelte', import.meta.url), 'utf8');

test('active invite copy labels disclose only the public access code', () => {
  assert.equal(publicInviteAccessCode(credential), ACCESS_CODE);
  assert.equal(inviteCopyTooltip(credential), `Room ID: ${ACCESS_CODE} (click to copy invite)`);
  assert.equal(inviteCopyAriaLabel(credential), `Room ID ${ACCESS_CODE}, click to copy invite`);
  assert.equal(inviteCopyTooltip('room-not-a-public-code'), 'Copy invite link');
  assert.equal(inviteCopyAriaLabel(null), 'Copy invite link');
});

test('desktop active-meeting invite controls receive the public-code labels on every density', () => {
  assert.match(routeSource, /const inviteAccessCode = \$derived\(meeting\.joinedRoom\?\.accessCode \|\| accessCodeForCredential\(roomName\)\)/);
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
