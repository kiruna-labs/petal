import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { copyRecentRoomInviteLink } from '../src/homeScreen.ts';
import { inviteLinkCopiedToastMessage } from '../src/inviteToast.ts';
import {
  accessCodeForCredential,
  internalCredentialForAccessCode,
  registerAccessCodeForCredential,
} from '@petal/shared/logic/meetingCode';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);
const homeScreenSource = readFileSync(new URL('../src/homeScreen.ts', import.meta.url), 'utf8');
const mainSource = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');

test('home setup assigns the profile color before the one-shot onboarding decision', () => {
  const ensureIndex = homeScreenSource.indexOf('let selectedColorIndex = ensureStoredColorIndex(localStorage);');
  const showOnboarding = homeScreenSource.indexOf('showProfileOnboarding();');

  assert.ok(ensureIndex >= 0, 'setupHomeScreen must initialize the stored color');
  assert.ok(showOnboarding > ensureIndex, 'the onboarding decision must follow color initialization');
});

test('profile onboarding requires a name and keeps color selection inside the modal', () => {
  assert.match(homeScreenSource, /openProfileColorPopover\(\)/);
  assert.match(homeScreenSource, /closeProfileColorPopover\(true\)/);
  assert.match(homeScreenSource, /event\.key === 'Escape'/);
  assert.match(homeScreenSource, /installDismissibleLayer/);
  assert.match(homeScreenSource, /profileOnboardingDone\?\.addEventListener\('click', \(\) => \{\s*if \(!displayNameInput\.value\.trim\(\)\) return;/);
  assert.doesNotMatch(homeScreenSource, /profileOnboardingSkip/);
  assert.doesNotMatch(homeScreenSource, /updateProfileColorPicker\(\);\s*hideProfileOnboarding\(\);/);
  assert.match(indexSource, /profile-color-bubble[\s\S]*aria-haspopup="dialog"[\s\S]*aria-controls="profile-color-options"/);
  assert.match(indexSource, /profile-color-options[\s\S]*role="dialog"[\s\S]*data-color-name="plum"/);
  assert.match(styleSource, /\.profile-color-picker\s*\{[\s\S]*position:\s*relative;/);
  assert.match(styleSource, /\.profile-color-options\s*\{[\s\S]*position:\s*absolute;/);
  assert.match(styleSource, /\.profile-color-options\[hidden\]\s*\{\s*display:\s*none;/);
});

test('main.ts never persists a color ahead of the first-visit onboarding check (ordering hazard guard)', () => {
  // main.ts must only ever READ the stored color (to decide whether to show
  // first-visit onboarding) before calling setupHomeScreen - it must never
  // WRITE one itself, since writing the color key ahead of that read would
  // make every genuinely-new visitor look "already assigned" and silently
  // suppress the first-visit popover forever. Only homeScreen.ts's
  // ensureStoredColorIndex (which runs inside setupHomeScreen, after this
  // check has already been evaluated) may write it.
  assert.ok(
    !mainSource.includes('ensureStoredColorIndex') && !mainSource.includes('saveStoredColorIndex('),
    'main.ts must not call ensureStoredColorIndex/saveStoredColorIndex directly - only homeScreen.ts may',
  );

  const storedProfileColorIndex = mainSource.indexOf('storedProfileColor');
  const setupHomeScreenIndex = mainSource.indexOf('setupHomeScreen(');

  assert.ok(storedProfileColorIndex >= 0, 'main.ts must read storedProfileColor to gate first-visit onboarding');
  assert.ok(setupHomeScreenIndex >= 0, 'main.ts must call setupHomeScreen');
  assert.ok(
    storedProfileColorIndex < setupHomeScreenIndex,
    'main.ts must capture storedProfileColor before calling setupHomeScreen (which is what persists a color)',
  );
});

test('recent-room copy action writes a labeled invite link and shows copied feedback', async () => {
  const writes: string[] = [];
  const toasts: string[] = [];
  const logs: Array<{ message: string; kind?: string }> = [];

  const url = await copyRecentRoomInviteLink({
    credential: CREDENTIAL,
    displayLabel: 'Design Review',
    origin: 'https://petal.example.com',
    clipboard: {
      async writeText(value: string) {
        writes.push(value);
      },
    },
    showToast: (message) => toasts.push(message),
    logEvent: (message, kind) => logs.push({ message, kind }),
  });

  assert.equal(url, `https://petal.example.com/design-review/${ACCESS_CODE}`);
  assert.deepEqual(writes, [url]);
  assert.deepEqual(toasts, [inviteLinkCopiedToastMessage(url)]);
  assert.deepEqual(logs, [{ message: `invite link copied: ${url}`, kind: 'ok' }]);
});

test('recent-room copy action falls back to toast and warning log when clipboard is unavailable', async () => {
  const toasts: string[] = [];
  const logs: Array<{ message: string; kind?: string }> = [];

  const url = await copyRecentRoomInviteLink({
    credential: CREDENTIAL,
    displayLabel: 'Design Review',
    origin: 'https://petal.example.com',
    clipboard: {
      async writeText() {
        throw new Error('clipboard blocked');
      },
    },
    showToast: (message) => toasts.push(message),
    logEvent: (message, kind) => logs.push({ message, kind }),
  });

  assert.deepEqual(toasts, [inviteLinkCopiedToastMessage(url)]);
  assert.deepEqual(logs, [{ message: `clipboard unavailable -- invite link: ${url}`, kind: 'warn' }]);
});

test('recent-room list renders a copy-invite control beside favorite', () => {
  assert.match(homeScreenSource, /copy\.className = 'recent-room__copy'/);
  assert.match(homeScreenSource, /const roomId = roomRecord\.accessCode \?\? accessCodeForCredential\(roomRecord\.code\)/);
  assert.match(homeScreenSource, /copy\.setAttribute\('aria-label', `Room ID \$\{roomIdLabel\}, click to copy invite`\)/);
  assert.match(homeScreenSource, /copy\.title = `Room ID: \$\{roomIdLabel\} \(click to copy invite\)`/);
  assert.match(homeScreenSource, /row\.append\(roomButton, copy, star\)/);
  assert.match(mainSource, /setupHomeScreen\(\{[\s\S]*showToast:\s*ctx\.ui\.showToast,[\s\S]*logEvent,/);
  assert.match(styleSource, /\.recent-room\s*{[\s\S]*grid-template-columns:\s*minmax\(0, 1fr\) 40px 40px;/);
  assert.match(styleSource, /\.recent-room:hover \.recent-room__copy,[\s\S]*\.recent-room__copy\.copied\s*{[\s\S]*opacity:\s*1;/);
});

// Regression test for a real production incident (2026-07-07): the
// credential -> access-code map is one-way and lives only for one page load.
// Rejoining a room via the recent-rooms list passes the internal credential
// directly (never re-typing/re-deriving the code), so on a FRESH page load
// -- one that never itself generated or parsed this exact code -- every
// invite-link builder silently degraded to the bare origin. Deliberately do
// NOT call internalCredentialForAccessCode/generateMeetingCode for this
// credential anywhere above (that side-effect-populates the map and would
// mask exactly the gap this test exists to catch).
test('a credential never generated or typed in this process cannot produce a real invite link until registered', async () => {
  const freshAccessCode = 'zzz-zyxw-zzz';
  const freshCredential = `room-${'9'.repeat(32)}`;

  assert.equal(
    accessCodeForCredential(freshCredential),
    null,
    'sanity check: this credential must not already be known, or the regression this test guards against would be masked'
  );

  const beforeRegistering = await copyRecentRoomInviteLink({
    credential: freshCredential,
    displayLabel: 'Ghost Room',
    origin: 'https://petal.example.com',
    clipboard: { async writeText() {} },
  });
  assert.equal(
    beforeRegistering,
    'https://petal.example.com/',
    'reproduces the incident: an unregistered credential degrades to the bare origin'
  );

  registerAccessCodeForCredential(freshCredential, freshAccessCode);

  const afterRegistering = await copyRecentRoomInviteLink({
    credential: freshCredential,
    displayLabel: 'Ghost Room',
    origin: 'https://petal.example.com',
    clipboard: { async writeText() {} },
  });
  assert.equal(afterRegistering, `https://petal.example.com/ghost-room/${freshAccessCode}`);
});

test('recent rooms persist their access code and re-seed the credential map on load', () => {
  assert.match(
    homeScreenSource,
    /import \{ accessCodeForCredential, registerAccessCodeForCredential \} from '@petal\/shared\/logic\/meetingCode';/
  );
  assert.match(
    homeScreenSource,
    /if \(typeof record\.accessCode === 'string'\) \{\s*registerAccessCodeForCredential\(code, record\.accessCode\);/
  );
  assert.match(
    homeScreenSource,
    /const accessCode = accessCodeForCredential\(normalized\) \?\? previous\?\.accessCode;/
  );
  assert.match(homeScreenSource, /accessCode: accessCode \?\? undefined,/);
});
