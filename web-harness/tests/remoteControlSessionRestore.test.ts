// #808 regression: a `stopped` status for a SUPERSEDED request cleared
// `state.activeRemoteControl`, and adoption of every later `active` was gated
// on that same object being non-null -- so once cleared, nothing could restore
// it. The controller sat permanently out of control while the host kept
// granting.
//
// Measured live (2026-08-14, rc-live-suite): four cases failed with
// `{granted:false, grantToken:null, tokenlessInputs:0}`, `active=null`, and a
// status pair two milliseconds apart -- `stopped` then `active` -- against a
// host that had just logged `status emitted (local+controller)
// status='active'`. The same gap silently defeated #371's reconnect re-emit,
// whose entire purpose is to restore a controller whose data channel was
// recreated.
//
// These tests pin the RESTORE decision, including the security conditions it
// must not relax: `targetUserId`/`windowId` on the wire are attacker-
// controlled and are not authentication.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { remoteControlStatusRestoresSession } from '../src/remoteControl.ts';
import type { RemoteControlStatusMessage } from '../src/trackNames.ts';

const LOCAL = 'web-controller';
const HOST = 'native-host';

function activeStatus(
  overrides: Partial<RemoteControlStatusMessage> = {}
): RemoteControlStatusMessage {
  return {
    v: 1,
    kind: 'status',
    targetUserId: LOCAL,
    controllerId: HOST,
    windowId: 6310,
    seq: 1786741465183,
    status: 'active',
    message: 'Remote control active for shared window',
    grantToken: '0123456789abcdef0123456789abcdef',
    controlSessionId: '0123456789abcdef0123456789abcdef',
    targetKind: 'window',
    shareInstanceId: 'share-instance-808',
    ...overrides
  } as RemoteControlStatusMessage;
}

const CONTEXT = {
  hasActiveSession: false,
  localIdentity: LOCAL,
  senderIdentity: HOST,
  tileOwner: HOST
};

test('an active status restores a session the controller no longer has (#808)', () => {
  assert.equal(remoteControlStatusRestoresSession(activeStatus(), CONTEXT), true);
});

test('a session that already exists is updated, never re-created', () => {
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, hasActiveSession: true }),
    false
  );
});

test('restore requires the LiveKit-verified sender to own the tile', () => {
  // The whole attack this guard closes: a room peer publishing a status that
  // names our identity, for a window it does not share.
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, senderIdentity: 'attacker' }),
    false
  );
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, tileOwner: 'someone-else' }),
    false
  );
  // No tile for that window at all -- nothing to restore against.
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, tileOwner: undefined }),
    false
  );
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, senderIdentity: undefined }),
    false
  );
});

test('restore requires the status to be addressed to us', () => {
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus({ targetUserId: 'other-controller' }), CONTEXT),
    false
  );
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus(), { ...CONTEXT, localIdentity: undefined }),
    false
  );
});

test('a tokenless active status restores nothing', () => {
  // #580's rule, carried into the restore path: without a token every input
  // packet is dropped by the host, so a session restored without one would be
  // a lie in exactly the way this whole class of bug was hard to see.
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus({ grantToken: undefined }), CONTEXT),
    false
  );
  assert.equal(remoteControlStatusRestoresSession(activeStatus({ grantToken: '' }), CONTEXT), false);
});

test('only an active status restores -- never stopped or a failure status', () => {
  for (const status of ['stopped', 'requestFailed', 'textTruncated', 'unavailable']) {
    assert.equal(
      remoteControlStatusRestoresSession(
        activeStatus({ status } as Partial<RemoteControlStatusMessage>),
        CONTEXT
      ),
      false,
      `status ${status} must not restore a session`
    );
  }
});

test('a half-formed v2 envelope does not restore', () => {
  // Same envelope rule the update path uses: an envelope that exists must be
  // whole, or the session it establishes would carry a target we cannot name.
  assert.equal(
    remoteControlStatusRestoresSession(activeStatus({ shareInstanceId: undefined }), CONTEXT),
    false
  );
  assert.equal(
    remoteControlStatusRestoresSession(
      activeStatus({ targetKind: undefined, shareInstanceId: 'share-instance-808' }),
      CONTEXT
    ),
    false
  );
  // A v1 host sends no envelope at all -- that is still restorable.
  assert.equal(
    remoteControlStatusRestoresSession(
      activeStatus({ targetKind: undefined, shareInstanceId: undefined, hostCapabilities: undefined }),
      CONTEXT
    ),
    true
  );
});
