import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const uiHelpersSource = readFileSync(new URL('../src/ui/uiHelpers.ts', import.meta.url), 'utf8');
const createJoinActionSource = readFileSync(new URL('../src/createJoinAction.ts', import.meta.url), 'utf8');
const connectionSource = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');

test('web meeting-name header actions and elapsed time are reserved and reveal on topbar hover/focus', () => {
  assert.match(
    indexSource,
    /<div class="room-title">[\s\S]*id="room-name"[\s\S]*id="room-copy"[\s\S]*id="room-rename"[\s\S]*id="elapsed"/
  );
  assert.match(
    styleSource,
    /\.room-title-actions\s*{[\s\S]*width:\s*52px;[\s\S]*opacity:\s*0;[\s\S]*pointer-events:\s*none;/
  );
  assert.match(
    styleSource,
    /\.topbar:hover \.room-title-actions,\s*\.room-title:focus-within \.room-title-actions\s*{[\s\S]*opacity:\s*1;[\s\S]*pointer-events:\s*auto;/
  );
  assert.match(
    styleSource,
    /\.elapsed\s*{[\s\S]*font-variant-numeric:\s*tabular-nums;[\s\S]*opacity:\s*0;/
  );
  assert.match(
    styleSource,
    /\.topbar:hover \.elapsed,\s*\.room-title:focus-within \.elapsed\s*{[\s\S]*opacity:\s*1;/
  );
  assert.match(styleSource, /color:\s*rgba\(255,\s*255,\s*255,\s*0\.5\);/);
  assert.match(
    styleSource,
    /\.room-title-actions\s*{[\s\S]*transition:\s*opacity var\(--motion-fast\) var\(--ease-standard\);[\s\S]*\.elapsed\s*{[\s\S]*transition:\s*opacity var\(--motion-fast\) var\(--ease-standard\);/
  );
  assert.doesNotMatch(styleSource, /\.room-title:hover \.room-title-actions/);
  assert.doesNotMatch(styleSource, /\.room-title:hover \.elapsed/);
  assert.match(
    styleSource,
    /\.room-name\.renaming \+ \.room-title-actions\s*{[\s\S]*opacity:\s*0;[\s\S]*pointer-events:\s*none;/
  );
});

test('web room title icon buttons remain matching square controls', () => {
  assert.match(
    styleSource,
    /\.room-title-action\s*{[\s\S]*width:\s*24px;[\s\S]*height:\s*24px;[\s\S]*border-radius:\s*var\(--radius-chip\);/
  );
});

test('web rename icon is backed by the existing room rename flow', () => {
  assert.match(controlsSource, /function startRoomRename\(\)/);
  assert.match(controlsSource, /input\.className = 'room-name-input'/);
  assert.match(controlsSource, /roomRenameButton\.addEventListener\('click', startRoomRename\)/);
  assert.match(controlsSource, /cb\.renameRoomDisplayName\(code, input\.value\)/);
});

test('web create-from-name stores the human label before connecting', () => {
  assert.match(controlsSource, /submitWebCreateJoinAction\(\{/);
  assert.match(createJoinActionSource, /newMeetingCredentialFromInput\(rawInput, credentialForNewMeeting\)/);
  assert.match(
    createJoinActionSource,
    /if \(displayName\) \{[\s\S]*renameRoomDisplayName\(code, displayName\);[\s\S]*\}[\s\S]*connectWithCredential\(code\)/
  );
});

test('web meeting title receives the server room display name after token minting', () => {
  assert.match(connectionSource, /newRoom\.connect\(tokenResponse\.url, tokenResponse\.token\)/);
  assert.match(connectionSource, /showMeetingScreen\(meetingCode,\s*tokenResponse\.displayName\)/);
});

test('web standalone invite button updates to disclose the active meeting access code', () => {
  assert.match(
    indexSource,
    /<button id="ctl-invite" class="control-button" type="button" aria-label="Copy invite link">/
  );
  assert.match(
    indexSource,
    /<span id="ctl-invite-tooltip" class="control-tooltip invite-control-tooltip" aria-hidden="true">Invite<\/span>/
  );
  assert.match(uiHelpersSource, /function setInviteCopyControls\(code: string \| null\)/);
  assert.match(uiHelpersSource, /setInviteCopyControls\(code\);/);
  assert.match(controlsSource, /ctlInvite\.addEventListener\('click', \(\) => \{[\s\S]*void copyCurrentInviteLink\(\);/);
});
