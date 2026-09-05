import assert from 'node:assert/strict';
import test from 'node:test';

import { submitWebCreateJoinAction } from '../src/createJoinAction.ts';
import { internalCredentialForAccessCode } from '@petal/shared/logic/meetingCode';
import { roomDisplayLabelForCredential } from '../src/roomLabels.ts';

const ACCESS_CODE = 'joa-uozn-rxt';
const CREDENTIAL = internalCredentialForAccessCode(ACCESS_CODE);

function actionContext(inputValue: string, connectWithCredential: (code: string) => Promise<void>) {
  let clearedErrors = 0;
  const errors: string[] = [];
  const toasts: string[] = [];
  const logs: Array<[string, string]> = [];
  const connects: string[] = [];
  const renames: Array<[string, string | null]> = [];

  return {
    clears: () => clearedErrors,
    connects,
    errors,
    logs,
    renames,
    run: () =>
      submitWebCreateJoinAction({
        clearError: () => {
          clearedErrors += 1;
        },
        connectWithCredential: async (code) => {
          connects.push(code);
          await connectWithCredential(code);
        },
        credentialForNewMeeting: () => CREDENTIAL,
        logEvent: (message, level) => logs.push([message, level]),
        pendingRecentCredential: () => null,
        rawInput: inputValue,
        renameRoomDisplayName: (code, displayName) => {
          renames.push([code, displayName]);
          return displayName ?? 'Petal meeting';
        },
        showError: (message) => errors.push(message),
        showToast: (message) => toasts.push(message)
      }),
    toasts
  };
}

test('web harness create/join action routes petal.live URLs and surfaces join failures visibly', async () => {
  const action = actionContext(
    `https://meet.petal.live/petal-meeting/${ACCESS_CODE}`,
    async () => {
      throw new Error('network timeout fetching token');
    }
  );

  await action.run();

  assert.equal(action.clears(), 1);
  assert.deepEqual(action.connects, [CREDENTIAL]);
  assert.deepEqual(action.renames, []);
  assert.deepEqual(action.errors, [
    'Could not join the meeting. Petal could not reach the meeting server. Check your connection and try again.'
  ]);
  assert.deepEqual(action.toasts, action.errors);
  assert.equal(action.logs[0]?.[1], 'error');
  assert.match(action.logs[0]?.[0] ?? '', /join meeting failed: network timeout fetching token/);
});

test('web harness create/join action creates named meetings through generated credentials', async () => {
  const action = actionContext(' eng demo ', async () => {});

  await action.run();

  assert.deepEqual(action.connects, [CREDENTIAL]);
  assert.deepEqual(action.renames, [[CREDENTIAL, 'eng demo']]);
  assert.deepEqual(action.errors, []);
});

for (const input of ['', '   ']) {
  test(`web harness blank create uses the Petal meeting fallback for ${input ? 'whitespace-only' : 'empty'} input`, async () => {
    const action = actionContext(input, async () => {});

    await action.run();

    assert.deepEqual(action.connects, [CREDENTIAL]);
    assert.deepEqual(action.renames, []);
    assert.equal(roomDisplayLabelForCredential(CREDENTIAL), 'Petal meeting');
    assert.deepEqual(action.errors, []);
  });
}
