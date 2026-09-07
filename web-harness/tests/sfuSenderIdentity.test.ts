// kiruna-labs/petal#2: livekit-client emits `RoomEvent.DataReceived` with
// `participant: undefined` for every packet from a peer that is momentarily
// absent from `remoteParticipants` (the gap after that peer's full reconnect),
// dropping the identity the SFU stamped on the packet. These tests pin the
// capture mechanics against a fake engine that reproduces the SDK's own
// listener ordering: Room's listener is registered first and emits
// `DataReceived` synchronously from inside `dataPacketReceived`.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ENGINE_DATA_PACKET_RECEIVED_EVENT,
  createSfuSenderIdentityResolver,
  type DataPacketLike
} from '../src/sfuSenderIdentity.ts';

type Listener = (...args: unknown[]) => void;

class FakeEngine {
  readonly listeners = new Map<string, Listener[]>();
  on(event: string, listener: Listener) {
    this.listeners.set(event, [...(this.listeners.get(event) ?? []), listener]);
    return this;
  }
  prependListener(event: string, listener: Listener) {
    this.listeners.set(event, [listener, ...(this.listeners.get(event) ?? [])]);
    return this;
  }
  emit(event: string, ...args: unknown[]) {
    for (const listener of this.listeners.get(event) ?? []) listener(...args);
  }
}

function userPacket(identity: string, topic = 'petal.remote-control'): DataPacketLike {
  return { participantIdentity: identity, value: { case: 'user', value: { topic, payload: new Uint8Array([1]) } } } as DataPacketLike;
}

/** Wire a fake Room the way livekit-client does: its listener is registered BEFORE ours. */
function fakeRoom(engine: FakeEngine, known: Set<string>) {
  const delivered: Array<{ participant: string | undefined }> = [];
  engine.on(ENGINE_DATA_PACKET_RECEIVED_EVENT, (packet) => {
    const { participantIdentity, value } = packet as DataPacketLike;
    if (value?.case !== 'user') return;
    delivered.push({ participant: known.has(participantIdentity ?? '') ? participantIdentity : undefined });
  });
  return delivered;
}

test('the SFU-stamped identity is available inside DataReceived when the SDK resolved no participant', () => {
  const engine = new FakeEngine();
  const delivered = fakeRoom(engine, new Set());
  const resolver = createSfuSenderIdentityResolver();
  assert.equal(resolver.attach(engine), true);

  let seenInsideDataReceived: string | undefined = 'unset';
  // Simulate the harness's DataReceived handler reading the resolver from
  // inside Room's synchronous emit.
  engine.listeners.get(ENGINE_DATA_PACKET_RECEIVED_EVENT)!.push(() => {
    seenInsideDataReceived = resolver.take();
  });
  engine.emit(ENGINE_DATA_PACKET_RECEIVED_EVENT, userPacket('native-host'));

  assert.equal(delivered[0]?.participant, undefined, 'fixture: the SDK could not resolve the sender');
  assert.equal(seenInsideDataReceived, 'native-host');
});

test('take() is one-shot: nothing carries over to the next packet', () => {
  const engine = new FakeEngine();
  fakeRoom(engine, new Set());
  const resolver = createSfuSenderIdentityResolver();
  resolver.attach(engine);
  engine.emit(ENGINE_DATA_PACKET_RECEIVED_EVENT, userPacket('native-host'));
  assert.equal(resolver.take(), 'native-host');
  assert.equal(resolver.take(), undefined);
});

test('a non-user packet clears any pending identity rather than leaving a stale one', () => {
  const engine = new FakeEngine();
  fakeRoom(engine, new Set());
  const resolver = createSfuSenderIdentityResolver();
  resolver.attach(engine);
  engine.emit(ENGINE_DATA_PACKET_RECEIVED_EVENT, userPacket('native-host'));
  engine.emit(ENGINE_DATA_PACKET_RECEIVED_EVENT, { participantIdentity: 'native-host', value: { case: 'transcription' } });
  assert.equal(resolver.take(), undefined);
});

test('an empty stamped identity is not an identity', () => {
  const engine = new FakeEngine();
  const resolver = createSfuSenderIdentityResolver();
  resolver.attach(engine);
  engine.emit(ENGINE_DATA_PACKET_RECEIVED_EVENT, userPacket(''));
  assert.equal(resolver.take(), undefined);
});

test('attach is idempotent per engine object', () => {
  const engine = new FakeEngine();
  const resolver = createSfuSenderIdentityResolver();
  assert.equal(resolver.attach(engine), true);
  assert.equal(resolver.attach(engine), true);
  assert.equal(engine.listeners.get(ENGINE_DATA_PACKET_RECEIVED_EVENT)?.length, 1);
  // A recreated engine (Room.connect() replaces a closed one) gets its own listener.
  const recreated = new FakeEngine();
  assert.equal(resolver.attach(recreated), true);
  assert.equal(recreated.listeners.get(ENGINE_DATA_PACKET_RECEIVED_EVENT)?.length, 1);
});

test('without prependListener nothing is attached -- an appended listener would mislabel the NEXT packet', () => {
  const resolver = createSfuSenderIdentityResolver();
  assert.equal(resolver.attach({ on: () => undefined }), false);
  assert.equal(resolver.attach(undefined), false);
  assert.equal(resolver.attach(null), false);
  assert.equal(resolver.take(), undefined);
});
