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
  OPT_OUT_RETRY_MAX_MS,
  STREAM_OPTOUT_ATTRIBUTE,
  STREAM_OPTOUT_VALUE,
  STREAM_LIVE_ATTRIBUTE,
  attachLiveStream,
  OPT_OUT_NOT_SAVED_ERROR,
  beforeMicrophonePublish,
  createLiveStreamBanner,
  deriveLiveStreams,
  keepOffStorageKey,
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
  streamOptOutAttribute: { key: string; optedOut: string; optedIn: string; included: string[]; rules: string[] };
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
  assert.equal(optOutValue(false), fixture.optedIn, 'opting back in is an explicit value, not a deleted key');
  assert.equal(isOptedOut(fixture.optedIn), false);
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

test('the choice is remembered per room', () => {
  const storage = new FakeStorage();
  assert.equal(loadKeepOffLiveStreams(storage, 'room-a'), false);
  saveKeepOffLiveStreams(storage, 'room-a', true);
  assert.equal(storage.getItem('petal.keepOffLiveStreams:room-a'), '1');
  assert.equal(loadKeepOffLiveStreams(storage, 'room-a'), true);
  assert.equal(loadKeepOffLiveStreams(storage, 'room-b'), false, 'another room keeps its own choice');
  saveKeepOffLiveStreams(storage, 'room-a', false);
  assert.equal(storage.getItem(keepOffStorageKey('room-a')), null);
  assert.equal(loadKeepOffLiveStreams(null, 'room-a'), false);
});

test('the contract says browser guests can opt out before any stream, per room', () => {
  const rules = contracts.streamOptOutAttribute.rules.join('\n');
  assert.match(rules, /browser guest can opt out at any time, before any stream starts; the choice is kept per room/);
  assert.match(rules, /writes its opt-out before it publishes a microphone on join, and writes it again after every reconnect/);
});

class FakeRoom implements LiveStreamRoom {
  name = 'petal-room-1';
  handlers = new Map<RoomEvent, Set<() => void>>();
  writes: Array<Record<string, string>> = [];
  /** Writes to refuse, like a signal request that timed out. */
  failures = 0;
  /** Resolves the next write when set (a write held in flight). */
  hold: Promise<void> | null = null;
  localParticipant = {
    identity: 'web-riley',
    attributes: {} as Record<string, string>,
    setAttributes: async (attributes: Record<string, string>) => {
      this.writes.push(attributes);
      if (this.hold) await this.hold;
      if (this.failures > 0) {
        this.failures -= 1;
        throw new Error('request timeout');
      }
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

const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

function attach(room: FakeRoom, storage: FakeStorage) {
  const renders: Array<[boolean, boolean]> = [];
  let toggle: (keepOff: boolean) => void = () => {};
  let destroyed = false;
  const controller = attachLiveStream(room, {
    storage,
    // Retries back off in tests without waiting in real time.
    sleep: () => new Promise<void>((resolve) => setImmediate(resolve)),
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
  // Included by default: "0" is written on join.
  await flush();
  assert.deepEqual(room.writes, [{ [STREAM_OPTOUT_ATTRIBUTE]: '0' }]);
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
  await flush();
  assert.deepEqual(room.writes.at(-1), { [STREAM_OPTOUT_ATTRIBUTE]: '1' });
  assert.equal(storage.getItem(keepOffStorageKey(room.name)), '1');
  assert.deepEqual(renders.at(-1), [true, true]);
  await controller.setKeepOff(false);
  assert.deepEqual(room.writes.at(-1), { [STREAM_OPTOUT_ATTRIBUTE]: '0' });
  assert.equal(storage.getItem(keepOffStorageKey(room.name)), null);

  // The streamer leaves: the banner goes.
  room.remoteParticipants.delete('dave');
  room.emit(RoomEvent.ParticipantDisconnected);
  assert.deepEqual(renders.at(-1), [false, false]);
});

test('a remembered opt-out is written on connect, and detach cleans up', async () => {
  const room = new FakeRoom();
  const storage = new FakeStorage();
  storage.setItem(keepOffStorageKey(room.name), '1');
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

test('a guest can opt out before anyone goes live', async () => {
  const room = new FakeRoom();
  const storage = new FakeStorage();
  const { controller, renders, toggle } = attach(room, storage);
  assert.equal(controller.live, false);
  toggle(true);
  await flush();
  assert.deepEqual(room.writes, [{ [STREAM_OPTOUT_ATTRIBUTE]: '1' }]);
  assert.deepEqual(renders.at(-1), [false, true]);
  assert.equal(storage.getItem(keepOffStorageKey(room.name)), '1');
});

test('a failed opt-out write is retried until it sticks', async () => {
  const room = new FakeRoom();
  room.failures = 3;
  const storage = new FakeStorage();
  const { controller } = attach(room, storage);
  await controller.setKeepOff(true);
  assert.equal(room.writes.length, 4, 'three failures, then the write that sticks');
  assert.equal(await controller.optOutSettled(10), true);
  assert.ok(OPT_OUT_RETRY_MAX_MS >= 1000);
});

test('a reconnect writes the opt-out again', async () => {
  const room = new FakeRoom();
  const storage = new FakeStorage();
  storage.setItem(keepOffStorageKey(room.name), '1');
  attach(room, storage);
  await flush();
  assert.equal(room.writes.length, 1);
  room.emit(RoomEvent.Reconnected);
  await flush();
  assert.deepEqual(room.writes.at(-1), { [STREAM_OPTOUT_ATTRIBUTE]: '1' });
  assert.equal(room.writes.length, 2);
});

test('opting back in while the opt-out is in flight still writes the clear', async () => {
  const room = new FakeRoom();
  let release: () => void = () => {};
  room.hold = new Promise<void>((resolve) => (release = resolve));
  const { controller } = attach(room, new FakeStorage());
  const first = controller.setKeepOff(true);
  await flush();
  const second = controller.setKeepOff(false);
  room.hold = null;
  release();
  await Promise.all([first, second]);
  await flush();
  assert.deepEqual(room.writes.at(-1), { [STREAM_OPTOUT_ATTRIBUTE]: '0' }, 'the server must not keep the stale "1"');
});

test('a mic publish waits until the server has the opt-out', async () => {
  const room = new FakeRoom();
  let release: () => void = () => {};
  room.hold = new Promise<void>((resolve) => (release = resolve));
  const storage = new FakeStorage();
  storage.setItem(keepOffStorageKey(room.name), '1');
  const { controller } = attach(room, storage);
  let micMayPublish = false;
  const waiting = beforeMicrophonePublish().then(() => {
    micMayPublish = true;
  });
  await flush();
  assert.equal(micMayPublish, false, 'the opt-out is still in flight');
  release();
  await waiting;
  assert.equal(micMayPublish, true);
  controller.detach();
  // Nothing attached: nothing to wait for.
  await beforeMicrophonePublish();
});

/** Just enough DOM for the banner. */
class FakeElement {
  hidden = false;
  className = '';
  textContent = '';
  type = '';
  checked = false;
  dataset: Record<string, string> = {};
  children: Array<FakeElement | string> = [];
  attributes = new Map<string, string>();
  removed = false;
  readonly tag: string;
  constructor(tag: string) {
    this.tag = tag;
  }
  setAttribute(name: string, value: string) {
    this.attributes.set(name, value);
  }
  addEventListener() {}
  append(...children: Array<FakeElement | string>) {
    this.children.push(...children);
  }
  after(element: FakeElement) {
    this.next = element;
  }
  remove() {
    this.removed = true;
  }
  next: FakeElement | null = null;
}

test('the opt-out control shows with nothing live; the LIVE notice only while live', () => {
  const doc = { createElement: (tag: string) => new FakeElement(tag) };
  const topbar = new FakeElement('div');
  const view = createLiveStreamBanner(doc as unknown as Document, topbar as unknown as Element, () => {});
  const banner = topbar.next!;
  const [dot, text, label] = banner.children as FakeElement[];
  view.render(false, false);
  assert.equal(banner.hidden, false, 'the strip (and its checkbox) is always there');
  assert.equal(label.hidden, false);
  assert.equal(dot.hidden, true);
  assert.equal(text.hidden, true);
  assert.equal(banner.dataset.live, 'false');
  view.render(true, true);
  assert.equal(dot.hidden, false);
  assert.equal(text.hidden, false);
  assert.equal(banner.dataset.live, 'true');
  assert.equal((label.children[0] as FakeElement).checked, true);
});

test('an opted-out mic never publishes while the opt-out is not saved', async () => {
  const room = new FakeRoom();
  room.failures = Number.MAX_SAFE_INTEGER;
  const storage = new FakeStorage();
  storage.setItem(keepOffStorageKey(room.name), '1');
  const { controller } = attach(room, storage);
  await assert.rejects(beforeMicrophonePublish(20), { message: OPT_OUT_NOT_SAVED_ERROR });
  // Detaching wakes anyone still waiting instead of leaving them to time out.
  const waiting = controller.optOutSettled(60_000);
  controller.detach();
  assert.equal(await waiting, false);
});
