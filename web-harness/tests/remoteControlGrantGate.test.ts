// #802 regression: the controller's grant gate silently discarded every
// macOS grant token. A macOS host advertised `host_capabilities: []`, serde's
// `skip_serializing_if = "Vec::is_empty"` dropped the key from the wire, and
// the gate requires a NON-EMPTY `hostCapabilities` exactly when
// `controlSessionId` is set -- i.e. exactly when a grant EXISTS. Every
// successful grant tripped the one condition it could never satisfy, with no
// error logged on either side. Live symptom: 30/30 remote-control cases
// failing at `request -> active status` with `granted: false, grantToken:
// null, tokenlessInputs: 0`.
//
// The host-side fix makes macOS advertise its real replay capabilities. These
// tests pin the wire shape BOTH sides must agree on, so the next divergence
// fails here instead of in a live suite.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import {
  remoteControlGrantEnvelopeIsValid,
  remoteControlNegotiatedGrantMatchesRequest
} from '../src/remoteControl.ts';
import type { RemoteControlStatusMessage } from '../src/trackNames.ts';

const CONTRACT = JSON.parse(
  readFileSync(fileURLToPath(new URL('../../contracts/petal-contracts.json', import.meta.url)), 'utf8')
) as {
  remoteControlMessages?: Array<{ name: string; message: Record<string, unknown> }>;
};

const REQUESTED = { targetKind: 'window' as const, shareInstanceId: 'share-instance-802' };

/** A status packet shaped exactly as a macOS host emits it for an active window grant. */
function macosActiveStatus(
  overrides: Partial<RemoteControlStatusMessage> = {}
): RemoteControlStatusMessage {
  return {
    v: 1,
    kind: 'status',
    targetUserId: 'web-controller',
    controllerId: 'native-host',
    windowId: 4828,
    seq: 12,
    status: 'active',
    message: 'Remote control active for shared window',
    grantToken: '0123456789abcdef0123456789abcdef',
    controlSessionId: '0123456789abcdef0123456789abcdef',
    targetKind: 'window',
    shareInstanceId: 'share-instance-802',
    hostCapabilities: [
      'discretePointerV1',
      'discreteScrollV1',
      'windowLocalPointer',
      'uiaInvoke',
      'uiaScroll',
      'globalKeyboard',
      'unicodeText'
    ],
    resultCapability: {
      version: 2,
      retryEnabled: false,
      retryDeadlineMs: 0,
      dedupGuaranteeWindowMs: 1000
    },
    supportsBinaryHotPath: true,
    ...overrides
  } as unknown as RemoteControlStatusMessage;
}

/** The same packet with one key absent, as an old host's wire shape would be. */
function without(
  message: RemoteControlStatusMessage,
  key: 'hostCapabilities' | 'controlSessionId'
): RemoteControlStatusMessage {
  const copy = { ...(message as unknown as Record<string, unknown>) };
  delete copy[key];
  return copy as unknown as RemoteControlStatusMessage;
}

test('the grant gate accepts a macOS-shaped active status packet', () => {
  const message = macosActiveStatus();
  assert.equal(remoteControlGrantEnvelopeIsValid(message), true);
  assert.equal(remoteControlNegotiatedGrantMatchesRequest(message, REQUESTED), true);
});

test('a host advertising no capabilities is still rejected — the gate itself is unchanged', () => {
  // This is the pre-fix macOS wire shape: `hostCapabilities` omitted entirely
  // because the host's vec was empty. It must still fail, so the assertion
  // above is proof the HOST changed, not proof the gate was loosened.
  const omitted = without(macosActiveStatus(), 'hostCapabilities');
  assert.equal(remoteControlNegotiatedGrantMatchesRequest(omitted, REQUESTED), false);

  assert.equal(
    remoteControlNegotiatedGrantMatchesRequest(macosActiveStatus({ hostCapabilities: [] } as never), REQUESTED),
    false
  );
});

test('a grantless status is adopted without any capability negotiation', () => {
  // Legacy hosts send no controlSessionId; the v2 negotiation must not apply.
  const legacy = without(macosActiveStatus(), 'controlSessionId');
  assert.equal(remoteControlNegotiatedGrantMatchesRequest(legacy, REQUESTED), true);
});

test('the negotiated envelope must match the target we actually requested', () => {
  assert.equal(
    remoteControlNegotiatedGrantMatchesRequest(macosActiveStatus(), {
      targetKind: 'window',
      shareInstanceId: 'a-different-share'
    }),
    false
  );
  assert.equal(
    remoteControlNegotiatedGrantMatchesRequest(macosActiveStatus(), {
      targetKind: 'display',
      shareInstanceId: REQUESTED.shareInstanceId
    }),
    false
  );
});

test('a v1-only result capability is rejected', () => {
  assert.equal(
    remoteControlNegotiatedGrantMatchesRequest(
      macosActiveStatus({
        resultCapability: { version: 1, retryEnabled: false }
      } as never),
      REQUESTED
    ),
    false
  );
});

test('the contract fixture for a capable active status passes the gate', () => {
  const fixture = CONTRACT.remoteControlMessages?.find(
    (packet) => packet.name === 'status-active-capable-window'
  );
  assert.ok(fixture, 'contract must carry a status-active-capable-window fixture');
  const message = {
    ...fixture.message,
    // The fixture predates controlSessionId/resultCapability being folded into
    // the same packet; add them so this exercises the v2 branch rather than
    // trivially passing through the `!controlSessionId` short-circuit.
    controlSessionId: fixture.message.grantToken,
    resultCapability: { version: 2, retryEnabled: false }
  } as unknown as RemoteControlStatusMessage;
  assert.equal(
    remoteControlNegotiatedGrantMatchesRequest(message, {
      targetKind: fixture.message.targetKind as 'window',
      shareInstanceId: fixture.message.shareInstanceId as string
    }),
    true
  );
});
