import { test } from 'node:test';
import assert from 'node:assert/strict';

import { inviteLinkForCredential, newMeetingCredentialFromInput } from '../src/controls.ts';
import { HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY } from '../src/constants.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import {
  roomDisplayLabelForCredential,
  roomDisplayLabelForCredentialWithMetadata,
  roomDisplayLabelForCredentialWithDisplayName,
  roomDisplayNameFromMetadata,
  roomFallbackLabelForCredential,
  setRoomDisplayLabel,
} from '../src/roomLabels.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

class MemoryStorage implements Pick<Storage, 'getItem' | 'setItem'> {
  values = new Map<string, string>();

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

test('room display labels fall back to a neutral label', () => {
  const storage = new MemoryStorage();

  assert.equal(roomFallbackLabelForCredential(CREDENTIAL), 'Petal meeting');
  assert.equal(roomDisplayLabelForCredential(CREDENTIAL, storage), 'Petal meeting');
});

test('local room rename overrides fallback and blank rename clears it', () => {
  const storage = new MemoryStorage();

  assert.equal(setRoomDisplayLabel(CREDENTIAL, 'Design Review', storage), 'Design Review');
  assert.equal(roomDisplayLabelForCredential(CREDENTIAL.toUpperCase(), storage), 'Design Review');

  assert.equal(setRoomDisplayLabel(CREDENTIAL, '   ', storage), 'Petal meeting');
  assert.equal(roomDisplayLabelForCredential(CREDENTIAL, storage), 'Petal meeting');
});

test('room metadata display name overrides local-only labels', () => {
  const storage = new MemoryStorage();
  setRoomDisplayLabel(CREDENTIAL, 'Local Only', storage);

  const metadata = JSON.stringify({ displayName: 'Eng meeting', open: true });

  assert.equal(roomDisplayNameFromMetadata(metadata), 'Eng meeting');
  assert.equal(roomDisplayLabelForCredentialWithMetadata(CREDENTIAL, metadata, storage), 'Eng meeting');
});

test('server token display name overrides local-only labels', () => {
  const storage = new MemoryStorage();
  setRoomDisplayLabel(CREDENTIAL, 'Local Only', storage);

  assert.equal(
    roomDisplayLabelForCredentialWithDisplayName(CREDENTIAL, 'Eng meeting', storage),
    'Eng meeting'
  );
});

test('room metadata title falls back when absent or invalid', () => {
  const storage = new MemoryStorage();
  setRoomDisplayLabel(CREDENTIAL, 'Local Only', storage);

  assert.equal(roomDisplayLabelForCredentialWithMetadata(CREDENTIAL, null, storage), 'Local Only');
  assert.equal(roomDisplayLabelForCredentialWithMetadata(CREDENTIAL, '{not-json', storage), 'Local Only');
  assert.equal(
    roomDisplayLabelForCredentialWithMetadata(CREDENTIAL, JSON.stringify({ displayName: '   ' }), storage),
    'Local Only'
  );
});

test('create-from-name preserves the human label separately from the credential slug', () => {
  const storage = new MemoryStorage();
  const created = newMeetingCredentialFromInput(' eng sync ', (label) => {
    assert.equal(label, 'eng sync');
    return CREDENTIAL;
  });

  assert.deepEqual(created, { code: CREDENTIAL, displayName: 'eng sync' });
  assert.equal(roomDisplayLabelForCredential(created.code, storage), 'Petal meeting');
  assert.equal(setRoomDisplayLabel(created.code, created.displayName, storage), 'eng sync');
  assert.equal(roomDisplayLabelForCredential(created.code, storage), 'eng sync');
});

test('empty create keeps the generated fallback label instead of storing a display override', () => {
  const created = newMeetingCredentialFromInput('   ', (label) => {
    assert.equal(label, undefined);
    return CREDENTIAL;
  });

  assert.deepEqual(created, { code: CREDENTIAL, displayName: null });
});

test('corrupt room-label storage is tolerated', () => {
  const storage = new MemoryStorage();
  storage.setItem(HARNESS_ROOM_DISPLAY_NAMES_STORAGE_KEY, '{not-json');

  assert.equal(roomDisplayLabelForCredential(CREDENTIAL, storage), 'Petal meeting');
  assert.equal(setRoomDisplayLabel(CREDENTIAL, 'Renamed', storage), 'Renamed');
});

test('room rename never changes invite credential', () => {
  const storage = new MemoryStorage();
  setRoomDisplayLabel(CREDENTIAL, 'Renamed Room', storage);

  const link = inviteLinkForCredential(CREDENTIAL, 'https://petal.example.com', 'Renamed Room');
  assert.equal(link, `https://petal.example.com/renamed-room/${ACCESS_CODE}`);
});
