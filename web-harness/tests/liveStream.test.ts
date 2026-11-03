// Live streams on the web: the LIVE derivation and the opt-out attribute
// are pinned to the same contract fixture as the desktop's
// `streaming::wire` (contracts/petal-contracts.json), and the controller is
// driven through a fake room and view: what it writes, when, and what it
// remembers.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { RoomEvent } from 'livekit-client';

import {
  KEEP_OFF_LIVE_STREAMS_STORAGE_KEY,
  STREAM_OPTOUT_ATTRIBUTE,
  STREAM_OPTOUT_VALUE,
  STREAM_LIVE_ATTRIBUTE,
  attachLiveStream,
  deriveLiveStreams,
  isOptedOut,
  loadKeepOffLiveStreams,
  optOutValue,
  parseLiveAttribute,
  saveKeepOffLiveStreams,
  type LiveStream,
  type LiveStreamRoom,
} from '../src/liveStream.ts';

const contracts = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8'),
) as {
  streamLiveDerivation: {
    attributes: Array<{ identity: string; value: string | null }>;
    streams: LiveStream[];
  };
  streamOptOutAttribute: { key: string; optedOut: string; included: string[] };
};

test('LIVE derivation matches the desktop on the shared fixture', () => {
  const { attributes, streams } = contracts.streamLiveDerivation;
  assert.deepEqual(deriveLiveStreams(attributes), streams);
});

test('a petal.live value is rejected exactly when the desktop rejects it', () => {
  const ok = { mode: 'client', startedAt: 1, windowId: 2, state: 'live' };
  assert.deepEqual(parseLiveAttribute(JSON.stringify(ok)), { startedAt: 1, windowId: 2, paused: false });
  for (const bad of [
    { ...ok, mode: 'server' },
    { ...ok, state: 'stopped' },
    { ...ok, startedAt: -1 },
    { ...ok, startedAt: 1.5 },
    { ...ok, windowId: 2 ** 32 },
    { ...ok, windowId: '2' },
    [ok],
  ]) {
    assert.equal(parseLiveAttribute(JSON.stringify(bad)), null, JSON.stringify(bad));
  }
  assert.equal(parseLiveAttribute(''), null);
  assert.equal(parseLiveAttribute(undefined), null);
});

test('only the exact value "1" opts out, as the contract says', () => {
  const fixture = contracts.streamOptOutAttribute;
  assert.equal(STREAM_OPTOUT_ATTRIBUTE, fixture.key);
  assert.equal(STREAM_OPTOUT_VALUE, fixture.optedOut);
  assert.equal(isOptedOut(fixture.optedOut), true);
  for (const value of fixture.included) assert.equal(isOptedOut(value), false, JSON.stringify(value));
  assert.equal(isOptedOut(undefined), false);
  assert.equal(optOutValue(true), '1');
  assert.equal(optOutValue(false), '', 'an empty value deletes the key');
});

class FakeStorage {
  items = new Map<string, string>();
  getItem(key: string) {
    return this.items.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    this.items.set(key, value);
  }
  removeItem(key: string) {
    this.items.delete(key);
  }
}

test('the choice is remembered under petal.keepOffLiveStreams', () => {
  const storage = new FakeStorage();
  assert.equal(loadKeepOffLiveStreams(storage), false);
  saveKeepOffLiveStreams(storage, true);
  assert.equal(storage.getItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY), '1');
  assert.equal(loadKeepOffLiveStreams(storage), true);
  saveKeepOffLiveStreams(storage, false);
  assert.equal(storage.getItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY), null);
  assert.equal(loadKeepOffLiveStreams(null), false);
});

class FakeRoom implements LiveStreamRoom {
  handlers = new Map<RoomEvent, Set<() => void>>();
  writes: Array<Record<string, string>> = [];
  localParticipant = {
    identity: 'web-riley',
    attributes: {} as Record<string, string>,
    setAttributes: async (attributes: Record<string, string>) => {
      this.writes.push(attributes);
    },
  };
  remoteParticipants = new Map<string, { identity: string; attributes: Record<string, string> }>();
  on(event: RoomEvent, listener: () => void) {
    if (!this.handlers.has(event)) this.handlers.set(event, new Set());
    this.handlers.get(event)!.add(listener);
  }
  off(event: RoomEvent, listener: () => void) {
    this.handlers.get(event)?.delete(listener);
  }
  emit(event: RoomEvent) {
    for (const listener of this.handlers.get(event) ?? []) listener();
  }
  listenerCount() {
    return [...this.handlers.values()].reduce((sum, set) => sum + set.size, 0);
  }
}

function attach(room: FakeRoom, storage: FakeStorage) {
  const renders: Array<[boolean, boolean]> = [];
  let toggle: (keepOff: boolean) => void = () => {};
  let destroyed = false;
  const controller = attachLiveStream(room, {
    storage,
    createView: (onToggle) => {
      toggle = onToggle;
      return {
        render: (live, keepOff) => renders.push([live, keepOff]),
        destroy: () => {
          destroyed = true;
        },
      };
    },
  });
  return { controller, renders, toggle: (keepOff: boolean) => toggle(keepOff), destroyed: () => destroyed };
}

test('the banner follows the room and the toggle writes the attribute and remembers it', async () => {
  const room = new FakeRoom();
  const storage = new FakeStorage();
  const { controller, renders, toggle } = attach(room, storage);
  // Included by default: nothing is written on join.
  assert.deepEqual(room.writes, []);
  assert.deepEqual(renders.at(-1), [false, false]);

  // A desktop participant goes live.
  room.remoteParticipants.set('dave', {
    identity: 'dave',
    attributes: { [STREAM_LIVE_ATTRIBUTE]: JSON.stringify({ mode: 'client', startedAt: 5, windowId: 7, state: 'live' }) },
  });
  room.emit(RoomEvent.ParticipantAttributesChanged);
  assert.equal(controller.live, true);
  assert.deepEqual(renders.at(-1), [true, false]);

  // The guest opts out from the banner, then back in.
  toggle(true);
  await Promise.resolve();
  assert.deepEqual(room.writes, [{ [STREAM_OPTOUT_ATTRIBUTE]: '1' }]);
  assert.equal(storage.getItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY), '1');
  assert.deepEqual(renders.at(-1), [true, true]);
  await controller.setKeepOff(false);
  assert.deepEqual(room.writes.at(-1), { [STREAM_OPTOUT_ATTRIBUTE]: '' });
  assert.equal(storage.getItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY), null);

  // The streamer leaves: the banner goes.
  room.remoteParticipants.delete('dave');
  room.emit(RoomEvent.ParticipantDisconnected);
  assert.deepEqual(renders.at(-1), [false, false]);
});

test('a remembered opt-out is written on connect, and detach cleans up', async () => {
  const room = new FakeRoom();
  const storage = new FakeStorage();
  storage.setItem(KEEP_OFF_LIVE_STREAMS_STORAGE_KEY, '1');
  const { controller, destroyed } = attach(room, storage);
  await Promise.resolve();
  assert.deepEqual(room.writes, [{ [STREAM_OPTOUT_ATTRIBUTE]: '1' }]);
  assert.equal(controller.keepOff, true);
  assert.ok(room.listenerCount() > 0);
  controller.detach();
  assert.equal(room.listenerCount(), 0);
  assert.equal(destroyed(), true);
});

test('connection.ts attaches the banner once joined and detaches it on every exit', () => {
  const source = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');
  assert.match(source, /joined = true;[\s\S]*?startLiveStream\(newRoom\);/);
  assert.equal(source.match(/stopLiveStream\(\);/g)?.length, 3, 'start (replace), disconnect, failed join');
});
