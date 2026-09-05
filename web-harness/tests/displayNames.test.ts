import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';

import {
  displayNameFromInput,
  HARNESS_IDENTITY_STORAGE_KEY,
  resolveHarnessIdentity,
  tokenRequestBody,
} from '../src/controls.ts';
import { HARNESS_NAME_STORAGE_KEY } from '../src/constants.ts';
import { telepointerLabelForIdentity } from '../src/telepointerDisplay.ts';
import {
  displayNameForParticipant,
  cameraOffNameFallback,
  cameraOffNameLabel,
  initialsFor,
  nameChipLabel,
  participantDisplayName,
} from '../src/tiles.ts';

class MemoryStorage implements Pick<Storage, 'getItem' | 'setItem'> {
  values = new Map<string, string>();

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

test('participant labels prefer display name and never expose UUID-like identities', () => {
  const uuid = '249CB1A3-C4E5-4E73-A4FE-C958E8DB78EB';

  assert.equal(participantDisplayName(uuid, 'Till'), 'Till');
  assert.equal(displayNameForParticipant({ identity: uuid, name: 'Till' }), 'Till');
  assert.equal(participantDisplayName(uuid), 'Guest');
  assert.equal(participantDisplayName(uuid, uuid), 'Guest');
  assert.equal(participantDisplayName('alice-laptop'), 'alice-laptop');
});

test('telepointer labels use the resolved display name with Guest fallback', () => {
  const uuid = '249CB1A3-C4E5-4E73-A4FE-C958E8DB78EB';

  assert.equal(telepointerLabelForIdentity(uuid, 'Till'), 'Till');
  assert.equal(telepointerLabelForIdentity(uuid), 'Guest');
  assert.equal(telepointerLabelForIdentity('bob-browser'), 'bob-browser');
});

test('camera-off fallback initials ignore local-only chip suffixes', () => {
  assert.equal(initialsFor('C (you)'), 'C');
  assert.equal(initialsFor('Ada Lovelace (you)'), 'AL');
  assert.equal(initialsFor('(you)'), '?');
});

test('camera-off centered label uses full display name without local-only chip suffixes', () => {
  assert.equal(cameraOffNameLabel('C (you)'), 'C');
  assert.equal(cameraOffNameLabel('Ada Lovelace (you)'), 'Ada Lovelace');
  assert.equal(cameraOffNameLabel('(you)'), 'Guest');
});

test('camera-off centered fallback uses one grapheme', () => {
  assert.equal(cameraOffNameFallback('Grace Hopper'), 'G');
  assert.equal(cameraOffNameFallback(' 👩🏽‍💻 Ada '), '👩🏽‍💻');
  assert.equal(cameraOffNameFallback('(you)'), 'G');
});

test('name chip labels keep full local text separate from compact fallback text', () => {
  assert.equal(nameChipLabel('C', true), 'C (you)');
  assert.equal(nameChipLabel('C', true, 'compact'), 'C');
  assert.equal(nameChipLabel('Grace Hopper', false), 'Grace Hopper');
  assert.equal(nameChipLabel('Grace Hopper', false, 'compact'), 'G');
  assert.equal(nameChipLabel('C (you)', false), 'C (you)');
});

test('web join stores a human display name while returning a stable technical identity', () => {
  const storage = new MemoryStorage();
  const input = { value: ' Bob ' } as HTMLInputElement;

  const identity = resolveHarnessIdentity(input, storage, () => '00000000-1111-2222-3333-444444444444');
  const secondIdentity = resolveHarnessIdentity(input, storage, () => 'unused');

  assert.equal(input.value, 'Bob');
  assert.equal(storage.getItem(HARNESS_NAME_STORAGE_KEY), 'Bob');
  assert.equal(identity, 'web-00000000-1111-2222-3333-444444444444');
  assert.equal(storage.getItem(HARNESS_IDENTITY_STORAGE_KEY), identity);
  assert.equal(secondIdentity, identity);
  assert.notEqual(identity, input.value);
});

test('token request publishes displayName distinct from identity', () => {
  const identity = 'web-00000000-1111-2222-3333-444444444444';
  const displayName = displayNameFromInput(' Till ');

  assert.deepEqual(tokenRequestBody('sync-d2b74918d47535a952ce4d8d126cd61c', identity, displayName), {
    room: 'sync-d2b74918d47535a952ce4d8d126cd61c',
    identity,
    displayName: 'Till',
  });
});

test('token request carries the access code for a credential this session derived from an invite (contract fixture)', () => {
  const contracts = JSON.parse(
    readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
  ) as { closedRoomTokenRequest: { request: { room: string; identity: string; displayName: string; accessCode: string } } };
  const { request } = contracts.closedRoomTokenRequest;
  // Deriving the credential from the code (what every join vector does) is
  // what makes the code known to tokenRequestBody.
  assert.equal(internalCredentialForAccessCode(request.accessCode), request.room);
  assert.deepEqual(tokenRequestBody(request.room, request.identity, request.displayName), request);
});
