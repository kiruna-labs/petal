import type { RemoteControlMessage, RemoteControlStatusMessage } from './trackNames.ts';

// ---------------------------------------------------------------------------
// RC-N2W (journey RC-07, #819): the harness as a remote-control HOST.
//
// Everywhere else in this harness the browser is the CONTROLLER, and
// `handleRemoteControlPayload` returns early for anything that is not a
// `result` or a `status`. RC-N2W needs the other side: a NATIVE controller
// requests control of a share this peer published, and something has to answer
// the request and record what arrives.
//
// What this can and cannot prove, stated once so no caller has to infer it:
// it is a DELIVERY ledger. A browser cannot inject OS input, so nothing here
// says an input was applied, and this emulator never sends a `result` at all --
// least of all `outcome: 'applied'`, which would be a lie the native side would
// happily believe. What it does prove is the half a native<->native run cannot
// isolate: that the controller's request, grant handshake and input messages
// are well formed and arrive intact at an independent implementation.
//
// It is OFF unless a cockpit scenario turns it on. A harness peer that
// advertised itself as controllable to any room member by default would be a
// behaviour change to a deployed page, not a test fixture.
// ---------------------------------------------------------------------------

/** Bounded so a long run cannot grow the ledger without limit. */
export const HOST_LEDGER_MAX_INPUTS = 240;

const INPUT_KINDS = new Set(['pointer', 'key', 'text', 'wheel']);

export interface ReceivedControlInput {
  kind: string;
  action?: string;
  key?: string;
  seq: number;
  at: number;
}

export interface HostEmulationState {
  enabled: boolean;
  /** Identity of the controller currently holding the grant, if any. */
  controllerId: string | null;
  grantToken: string | null;
  received: ReceivedControlInput[];
  /** True once a request has been answered with an `active` status. */
  granted: boolean;
  /**
   * Error from publishing a grant/stop status, if any. A grant the
   * controller never HEARD is not a grant -- without this the native
   * controller's timeout reads as "the web peer never granted", a product
   * verdict manufactured by a swallowed transport error (#819 review).
   */
  publishError: string | null;
}

export type HostEmulationDecision =
  | { action: 'ignore'; reason: string }
  | { action: 'grant'; status: RemoteControlStatusMessage }
  | { action: 'record'; input: ReceivedControlInput }
  | { action: 'stop'; status: RemoteControlStatusMessage };

export function createHostEmulationState(): HostEmulationState {
  return {
    enabled: false,
    controllerId: null,
    grantToken: null,
    received: [],
    granted: false,
    publishError: null,
  };
}

function statusMessage(
  status: 'active' | 'stopped',
  message: RemoteControlMessage,
  localIdentity: string,
  controllerIdentity: string,
  grantToken: string | null
): RemoteControlStatusMessage {
  return {
    v: 1,
    kind: 'status',
    status,
    // For status packets the wire's `controllerId` carries the HOST's
    // identity and `targetUserId` the controller's -- the reverse of an input
    // packet. Getting this backwards makes the native controller drop the
    // packet as "not the window owner", silently and with no other symptom.
    controllerId: localIdentity,
    targetUserId: controllerIdentity,
    windowId: message.windowId,
    seq: Date.now(),
    message:
      status === 'active'
        ? 'Remote control granted by the web harness host emulator: it records input and never injects it'
        : 'Remote control released',
    ...(grantToken ? { grantToken } : {})
  };
}

/**
 * Decide what the emulated host does with one inbound message. Pure, so the
 * authorization rules below are testable without a room.
 *
 * `senderIdentity` is the LiveKit-authenticated publisher. Everything in the
 * message body is attacker-controlled and is never used for authorization --
 * the same rule the controller-side handlers in this harness already follow.
 */
export function hostEmulationDecision(
  state: HostEmulationState,
  message: RemoteControlMessage,
  localIdentity: string,
  senderIdentity: string | undefined
): HostEmulationDecision {
  if (!state.enabled) return { action: 'ignore', reason: 'host emulation is off' };
  if (!senderIdentity) return { action: 'ignore', reason: 'unauthenticated sender' };
  if (senderIdentity === localIdentity) return { action: 'ignore', reason: 'own packet' };
  if (message.targetUserId !== localIdentity) {
    return { action: 'ignore', reason: 'addressed to another peer' };
  }
  if (message.controllerId !== senderIdentity) {
    return { action: 'ignore', reason: 'controllerId does not match the authenticated sender' };
  }
  if (message.kind === 'request') {
    const grantToken = `rc-n2w-${message.windowId}-${Date.now().toString(36)}`;
    return {
      action: 'grant',
      status: statusMessage('active', message, localIdentity, senderIdentity, grantToken)
    };
  }
  if (message.kind === 'release') {
    return {
      action: 'stop',
      status: statusMessage('stopped', message, localIdentity, senderIdentity, null)
    };
  }
  if (!INPUT_KINDS.has(message.kind)) {
    return { action: 'ignore', reason: `kind ${message.kind} is not an input` };
  }
  if (!state.granted || state.controllerId !== senderIdentity) {
    return { action: 'ignore', reason: 'input from a controller holding no grant' };
  }
  return {
    action: 'record',
    input: {
      kind: message.kind,
      ...('action' in message ? { action: message.action } : {}),
      ...('key' in message ? { key: message.key } : {}),
      seq: message.seq,
      at: Date.now()
    }
  };
}

export function applyHostEmulationDecision(
  state: HostEmulationState,
  decision: HostEmulationDecision
): void {
  switch (decision.action) {
    case 'grant':
      state.granted = true;
      state.controllerId = decision.status.targetUserId;
      state.grantToken = decision.status.grantToken ?? null;
      break;
    case 'stop':
      state.granted = false;
      state.controllerId = null;
      state.grantToken = null;
      break;
    case 'record':
      if (state.received.length >= HOST_LEDGER_MAX_INPUTS) state.received.shift();
      state.received.push(decision.input);
      break;
    case 'ignore':
      break;
  }
}

/** Distinct wire kinds this peer received, in first-seen order. */
export function receivedControlKinds(state: HostEmulationState): string[] {
  const seen: string[] = [];
  for (const input of state.received) {
    if (!seen.includes(input.kind)) seen.push(input.kind);
  }
  return seen;
}
