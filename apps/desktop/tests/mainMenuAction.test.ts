import assert from 'node:assert/strict';
import test from 'node:test';

import { internalCredentialForAccessCode } from '../src/lib/data/meetingCode.ts';
import {
  INVALID_MEETING_INPUT_ERROR,
  submitMainMenuMeetingAction
} from '../src/lib/data/mainMenuMeetingAction.ts';
import { createMainMeetingActions } from '../src/lib/data/mainMeetingActions.ts';
import type { RoomRecord } from '../src/lib/ipc.ts';

const ACCESS_CODE = 'abc-defg-hjk';
const ALT_ACCESS_CODE = 'joa-uozn-rxt';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);
const ALT_CREDENTIAL = internalCredentialForAccessCode(ALT_ACCESS_CODE);

function roomRecord(name: string, displayName: string | null = null): RoomRecord {
  return {
    id: `id-${name}`,
    name,
    accessCode: ACCESS_CODE,
    displayName,
    slug: name,
    createdAtMs: 1,
    open: true
  };
}

test('main menu submit action routes empty input to generated-code create', async () => {
  const createCalls: Array<[string, string | null]> = [];

  const result = await submitMainMenuMeetingAction(
    '',
    {
      onCreateMeeting: async (name, displayName) => {
        createCalls.push([name, displayName]);
      }
    },
    () => ACCESS_CODE
  );

  assert.deepEqual(createCalls, [[ACCESS_CODE, null]]);
  assert.deepEqual(result, { action: 'create', error: null, nextInput: '', submitted: true });
});

test('main menu submit action treats typed names as display labels only', async () => {
  const createCalls: Array<[string, string | null]> = [];

  const result = await submitMainMenuMeetingAction(
    ' eng demo ',
    {
      onCreateMeeting: async (name, displayName) => {
        createCalls.push([name, displayName]);
      }
    },
    () => ACCESS_CODE
  );

  assert.deepEqual(createCalls, [[ACCESS_CODE, 'eng demo']]);
  assert.deepEqual(result, { action: 'create', error: null, nextInput: '', submitted: true });
});

test('main menu submit action routes bare access codes and petal.live URLs to join', async () => {
  const joinCalls: Array<[string, string]> = [];

  const bareResult = await submitMainMenuMeetingAction(
    ALT_ACCESS_CODE,
    {
      onJoinByCode: async (name, accessCode) => {
        joinCalls.push([name, accessCode]);
      }
    },
    () => ACCESS_CODE
  );

  const urlResult = await submitMainMenuMeetingAction(
    `https://meet.petal.live/petal-meeting/${ACCESS_CODE}`,
    {
      onJoinByCode: async (name, accessCode) => {
        joinCalls.push([name, accessCode]);
      }
    },
    () => ALT_ACCESS_CODE
  );

  assert.deepEqual(joinCalls, [
    [ALT_CREDENTIAL, ALT_ACCESS_CODE],
    [CREDENTIAL, ACCESS_CODE]
  ]);
  assert.deepEqual(bareResult, { action: 'join', error: null, nextInput: '', submitted: true });
  assert.deepEqual(urlResult, { action: 'join', error: null, nextInput: '', submitted: true });
});

test('main menu submit action surfaces invalid join-looking input without calling create', async () => {
  let createCalls = 0;
  let joinCalls = 0;

  const result = await submitMainMenuMeetingAction(
    'https://meet.petal.live/not/a/code',
    {
      onCreateMeeting: async () => {
        createCalls += 1;
      },
      onJoinByCode: async () => {
        joinCalls += 1;
      }
    },
    () => ACCESS_CODE
  );

  assert.equal(createCalls, 0);
  assert.equal(joinCalls, 0);
  assert.deepEqual(result, {
    action: null,
    error: INVALID_MEETING_INPUT_ERROR,
    nextInput: 'https://meet.petal.live/not/a/code',
    submitted: false
  });
});

test('main menu submit action maps callback rejection to user-visible create error', async () => {
  const result = await submitMainMenuMeetingAction(
    'eng demo',
    {
      onCreateMeeting: async () => {
        throw new Error('Failed to fetch token');
      }
    },
    () => ACCESS_CODE
  );

  assert.equal(result.action, 'create');
  assert.equal(result.nextInput, '');
  assert.equal(result.submitted, false);
  assert.equal(
    result.error,
    'Could not create the meeting. Petal could not reach the meeting server. Check your connection and try again.'
  );
});

test('main route create_room rejection sets inline error and does not navigate', async () => {
  const errors: Array<string | null> = [];
  const routes: string[] = [];
  const logs: Array<[string, unknown]> = [];
  const actions = createMainMeetingActions({
    createRoom: async () => {
      throw new Error('backend returned 500 while minting token');
    },
    goto: async (route) => {
      routes.push(route);
    },
    setMeetingActionError: (message) => {
      errors.push(message);
    },
    logger: {
      info: () => {},
      error: (message, detail) => logs.push([message, detail])
    }
  });

  await actions.startMeetingAndGo(ACCESS_CODE, 'Eng demo');

  assert.deepEqual(routes, []);
  assert.equal(errors[0], null);
  assert.equal(
    errors[1],
    'Could not create the meeting. Petal could not reach the meeting server. Check your connection and try again.'
  );
  assert.equal(logs[0]?.[0], 'Failed to start meeting');
});

test('main route join action creates an open room then navigates to the encoded meeting route', async () => {
  const createCalls: Array<[string, boolean, string | null | undefined]> = [];
  const routes: string[] = [];
  const actions = createMainMeetingActions({
    createRoom: async (name, open, displayName) => {
      createCalls.push([name, open, displayName]);
      return roomRecord('room with spaces');
    },
    goto: async (route) => {
      routes.push(route);
    },
    setMeetingActionError: () => {},
    logger: {
      info: () => {},
      error: () => {}
    }
  });

  await actions.joinAndGo(CREDENTIAL);

  assert.deepEqual(createCalls, [[CREDENTIAL, true, undefined]]);
  assert.deepEqual(routes, ['/meeting/room%20with%20spaces']);
});

test('paste-join forwards the pasted access code, never the pre-hashed credential (#421)', async () => {
  // The access code is the pre-image of the room capability, so the backend can
  // only store a re-shareable code if this seam hands it the code itself.
  // Passing the credential instead is what made the backend mint an unrelated
  // code, sending anyone who used the resulting invite into a different room.
  const createCalls: Array<[string, boolean, string | null | undefined]> = [];
  const actions = createMainMeetingActions({
    createRoom: async (name, open, displayName) => {
      createCalls.push([name, open, displayName]);
      return roomRecord(CREDENTIAL);
    },
    goto: async () => {},
    setMeetingActionError: () => {},
    logger: {
      info: () => {},
      error: () => {}
    }
  });

  await submitMainMenuMeetingAction(
    `https://meet.petal.live/petal-meeting/${ACCESS_CODE}`,
    { onJoinByCode: (name, accessCode) => actions.joinAndGo(name, accessCode) },
    () => ALT_ACCESS_CODE
  );

  assert.deepEqual(createCalls, [[ACCESS_CODE, true, undefined]]);
});
