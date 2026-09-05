import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  inviteCopyAriaLabel,
  inviteCopyTooltip,
  publicInviteAccessCode
} from '../src/inviteCopy.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';

const ACCESS_CODE = 'abc-defg-hjk';
const credential = internalCredentialForAccessCode(ACCESS_CODE);
const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const helperSource = readFileSync(new URL('../src/ui/uiHelpers.ts', import.meta.url), 'utf8');
const mainSource = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');

test('web active invite copy labels disclose only the public access code', () => {
  assert.equal(publicInviteAccessCode(credential), ACCESS_CODE);
  assert.equal(inviteCopyTooltip(credential), `Room ID: ${ACCESS_CODE} (click to copy invite)`);
  assert.equal(inviteCopyAriaLabel(credential), `Room ID ${ACCESS_CODE}, click to copy invite`);
  assert.equal(inviteCopyTooltip('room-not-a-public-code'), 'Copy invite link');
});

test('web active-meeting topbar and control-bar copy actions update together', () => {
  assert.match(indexSource, /id="room-copy"[\s\S]*aria-label="Copy invite link"/);
  assert.match(indexSource, /id="ctl-invite"[\s\S]*aria-label="Copy invite link"/);
  assert.match(indexSource, /id="ctl-invite-tooltip" class="control-tooltip invite-control-tooltip"/);
  assert.match(helperSource, /for \(const control of \[options\.roomCopyButton, options\.ctlInvite\]\) \{[\s\S]*control\.setAttribute\('aria-label', ariaLabel\);[\s\S]*control\.title = tooltip;/);
  assert.match(helperSource, /options\.ctlInviteTooltip\.textContent = tooltip;/);
  assert.match(helperSource, /showMeetingScreen\([\s\S]*setInviteCopyControls\(code\)/);
});

test('web invite tooltip has a readable viewport-bounded width and wraps its full text', () => {
  assert.match(
    styleSource,
    /\.invite-control-tooltip\s*\{[\s\S]*width:\s*min\(220px,\s*calc\(100vw\s*-\s*24px\)\);[\s\S]*box-sizing:\s*border-box;[\s\S]*white-space:\s*normal;[\s\S]*overflow-wrap:\s*anywhere;[\s\S]*text-wrap:\s*pretty;/
  );
  assert.match(styleSource, /\.control-cell:hover \.invite-control-tooltip,[\s\S]*transform:\s*translate\(calc\(-50% \+ var\(--invite-tooltip-shift, 0px\)\), 0\);/);
  assert.match(mainSource, /const INVITE_TOOLTIP_GUTTER_PX = 12;/);
  assert.match(mainSource, /const unshiftedLeft = rect\.left - inviteTooltipShift;[\s\S]*const unshiftedRight = rect\.right - inviteTooltipShift;/);
  assert.match(mainSource, /inviteTooltipShift = unshiftedLeft < INVITE_TOOLTIP_GUTTER_PX[\s\S]*unshiftedRight > window\.innerWidth - INVITE_TOOLTIP_GUTTER_PX/);
  assert.match(mainSource, /setProperty\('--invite-tooltip-shift', `\$\{inviteTooltipShift\}px`\)/);
  assert.match(mainSource, /ctlInviteCell\?\.addEventListener\('mouseenter', keepInviteTooltipInViewport\);/);
});
