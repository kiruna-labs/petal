import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  HOST_LEDGER_MAX_INPUTS,
  applyHostEmulationDecision,
  createHostEmulationState,
  hostEmulationDecision,
  receivedControlKinds,
  type HostEmulationState
} from '../src/remoteControlHostLedger.ts';
import type { RemoteControlMessage } from '../src/trackNames.ts';

const HOST = 'p-web-host';
const CONTROLLER = 'p-native-controller';

function request(overrides: Partial<RemoteControlMessage> = {}): RemoteControlMessage {
  return {
    v: 1,
    kind: 'request',
    targetUserId: HOST,
    controllerId: CONTROLLER,
    windowId: 42,
    seq: 1,
    ...overrides
  } as RemoteControlMessage;
}

function pointerDown(overrides: Partial<RemoteControlMessage> = {}): RemoteControlMessage {
  return {
    v: 1,
    kind: 'pointer',
    action: 'down',
    targetUserId: HOST,
    controllerId: CONTROLLER,
    windowId: 42,
    seq: 2,
    x: 0.5,
    y: 0.5,
    button: 0,
    buttons: 1,
    modifiers: { alt: false, ctrl: false, meta: false, shift: false },
    ...overrides
  } as RemoteControlMessage;
}

function granted(): HostEmulationState {
  const state = createHostEmulationState();
  state.enabled = true;
  applyHostEmulationDecision(state, hostEmulationDecision(state, request(), HOST, CONTROLLER));
  return state;
}

test('host emulation is off until a scenario turns it on', () => {
  const state = createHostEmulationState();
  const decision = hostEmulationDecision(state, request(), HOST, CONTROLLER);
  assert.equal(decision.action, 'ignore');
});

test('a control request is answered with an active status carrying a grant token', () => {
  const state = createHostEmulationState();
  state.enabled = true;
  const decision = hostEmulationDecision(state, request(), HOST, CONTROLLER);
  assert.equal(decision.action, 'grant');
  if (decision.action !== 'grant') return;
  assert.equal(decision.status.status, 'active');
  assert.ok(decision.status.grantToken, 'the controller drops tokenless grants');
  // For a status packet the wire's controllerId carries the HOST identity and
  // targetUserId the controller's. Reversed, the native controller drops the
  // packet as "not the window owner" with no other symptom.
  assert.equal(decision.status.controllerId, HOST);
  assert.equal(decision.status.targetUserId, CONTROLLER);
  assert.equal(decision.status.windowId, 42);
});

test('an input from a controller holding no grant is not recorded', () => {
  const state = createHostEmulationState();
  state.enabled = true;
  const decision = hostEmulationDecision(state, pointerDown(), HOST, CONTROLLER);
  assert.equal(decision.action, 'ignore');
});

test('inputs are recorded once the grant exists', () => {
  const state = granted();
  const decision = hostEmulationDecision(state, pointerDown(), HOST, CONTROLLER);
  assert.equal(decision.action, 'record');
  applyHostEmulationDecision(state, decision);
  assert.deepEqual(receivedControlKinds(state), ['pointer']);
  assert.equal(state.received[0].action, 'down');
});

test('an unauthenticated sender is refused even with a matching body', () => {
  const state = granted();
  const decision = hostEmulationDecision(state, pointerDown(), HOST, undefined);
  assert.equal(decision.action, 'ignore');
});

test('a request whose body names a controller other than the authenticated sender is refused', () => {
  // This is the case that isolates the sender check. Asserting it on an INPUT
  // instead proves nothing: an input from a stranger is already refused by the
  // grant-owner check, so that test stays green with the sender check deleted
  // -- a sibling guard rescuing the mutation rather than the guard under test.
  const state = createHostEmulationState();
  state.enabled = true;
  const decision = hostEmulationDecision(state, request(), HOST, 'p-someone-else');
  assert.equal(
    decision.action,
    'ignore',
    'a grant must never be issued for a request whose claimed controller is not the sender'
  );
});

test('an input from a stranger is refused even while another controller holds the grant', () => {
  const state = granted();
  const decision = hostEmulationDecision(state, pointerDown(), HOST, 'p-someone-else');
  assert.equal(decision.action, 'ignore');
});

test('a message addressed to a different peer is ignored', () => {
  const state = granted();
  const decision = hostEmulationDecision(
    state,
    pointerDown({ targetUserId: 'p-third-party' }),
    HOST,
    CONTROLLER
  );
  assert.equal(decision.action, 'ignore');
});

test('release stops the session and later inputs stop being recorded', () => {
  const state = granted();
  const stop = hostEmulationDecision(
    state,
    request({ kind: 'release' }) as RemoteControlMessage,
    HOST,
    CONTROLLER
  );
  assert.equal(stop.action, 'stop');
  applyHostEmulationDecision(state, stop);
  assert.equal(state.granted, false);
  assert.equal(hostEmulationDecision(state, pointerDown(), HOST, CONTROLLER).action, 'ignore');
});

test('the ledger is bounded and drops the oldest input first', () => {
  const state = granted();
  for (let seq = 0; seq < HOST_LEDGER_MAX_INPUTS + 3; seq += 1) {
    applyHostEmulationDecision(state, {
      action: 'record',
      input: { kind: 'key', seq, at: seq }
    });
  }
  assert.equal(state.received.length, HOST_LEDGER_MAX_INPUTS);
  assert.equal(state.received[0].seq, 3);
});

test('the emulator never produces a result, let alone an applied one', () => {
  // The distinction this whole module exists for: a browser cannot inject OS
  // input, so claiming an outcome would be a lie the native side believes.
  const state = granted();
  for (const message of [request(), pointerDown(), request({ kind: 'release' })]) {
    const decision = hostEmulationDecision(state, message as RemoteControlMessage, HOST, CONTROLLER);
    if (decision.action === 'grant' || decision.action === 'stop') {
      assert.equal(decision.status.kind, 'status');
      assert.ok(!('outcome' in decision.status));
    }
  }
});
