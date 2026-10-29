import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  IMMEDIATE_COPIES,
  INPUT_BUNDLE_KIND,
  INPUT_BUNDLE_VERSION,
  MAX_BUNDLE_INPUTS,
  MAX_BUNDLE_PAYLOAD_BYTES,
  MAX_REDUNDANT_AGE_MS,
  MAX_REDUNDANT_COPIES,
  MIN_BUNDLE_INTERVAL_MS,
  TRAILING_COPY_DELAYS_MS,
  RedundantInputSender,
  publishRedundantInput,
  ridesRedundantStream,
  type PushOutcome,
  type RemoteControlInputBundle
} from '../src/remoteControlRedundant.ts';
import { setupRemoteControlUi } from '../src/remoteControlUi.ts';
import type { HarnessContext } from '../src/context.ts';
import type { RemoteControlMessage } from '../src/trackNames.ts';

const contract = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
).remoteControlInputBundle as {
  kind: string;
  version: number;
  topic: string;
  reliable: boolean;
  capabilityField: string;
  maxRedundantCopies: number;
  maxBundleInputs: number;
  minBundleIntervalMs: number;
  immediateCopies: number;
  maxPayloadBytes: number;
  maxRedundantAgeMs: number;
  trailingCopyDelaysMs: number[];
  fields: string[];
  example: RemoteControlInputBundle;
};

const modifiers = { alt: false, ctrl: false, meta: false, shift: false };

function key(seq: number, action: 'down' | 'up', keyName = 'a'): RemoteControlMessage {
  return {
    v: 1,
    targetUserId: 'native-host',
    controllerId: 'web',
    windowId: 7,
    seq,
    grantToken: 'tok',
    kind: 'key',
    action,
    key: keyName,
    code: `Key${keyName.toUpperCase()}`,
    modifiers
  } as RemoteControlMessage;
}

/** Fake clock: timers run only when the test advances time. */
function fakeClock() {
  let now = 1_000;
  const timers: Array<{ at: number; fn: () => void }> = [];
  return {
    now: () => now,
    setTimeout: (fn: () => void, ms: number) => {
      timers.push({ at: now + ms, fn });
      return 0 as unknown as ReturnType<typeof setTimeout>;
    },
    advance(ms: number) {
      now += ms;
      for (const timer of timers.splice(0).sort((a, b) => a.at - b.at)) {
        if (timer.at <= now) timer.fn();
        else timers.push(timer);
      }
    }
  };
}

test('redundant input constants and wire shape match the shared contract', () => {
  assert.equal(contract.kind, INPUT_BUNDLE_KIND);
  assert.equal(contract.version, INPUT_BUNDLE_VERSION);
  assert.equal(contract.topic, 'petal.remote-control');
  assert.equal(contract.reliable, false);
  assert.equal(contract.capabilityField, 'supportsRedundantInput');
  assert.equal(contract.maxRedundantCopies, MAX_REDUNDANT_COPIES);
  assert.equal(contract.maxBundleInputs, MAX_BUNDLE_INPUTS);
  assert.equal(contract.minBundleIntervalMs, MIN_BUNDLE_INTERVAL_MS);
  assert.equal(contract.immediateCopies, IMMEDIATE_COPIES);
  assert.equal(contract.maxPayloadBytes, MAX_BUNDLE_PAYLOAD_BYTES);
  assert.equal(contract.maxRedundantAgeMs, MAX_REDUNDANT_AGE_MS);
  assert.deepEqual(contract.trailingCopyDelaysMs, [...TRAILING_COPY_DELAYS_MS]);
  assert.deepEqual(Object.keys(contract.example).sort(), contract.fields);

  const sender = new RedundantInputSender(42, contract.example.stream);
  for (let i = 1; i < contract.example.first; i += 1) sender.push(key(i, 'down'), i * 10);
  published(sender.push(contract.example.inputs[0], 1_000));
  const bundle = published(sender.push(contract.example.inputs[1], 1_010));
  assert.deepEqual(bundle, contract.example);
});

function published(outcome: PushOutcome): RemoteControlInputBundle {
  assert.equal(outcome.kind, 'publish');
  return (outcome as Extract<PushOutcome, { kind: 'publish' }>).bundle;
}

test('each bundle carries the newest input plus the ones before it, so one loss is recovered', () => {
  const sender = new RedundantInputSender(7, 's1');
  const bundles = [1, 2, 3].map((seq) => published(sender.push(key(seq, 'down'), seq * 10)));
  // If bundles[1] is lost, bundles[2] still carries seq 2.
  assert.equal(bundles[2].first, 1);
  assert.deepEqual(bundles[2].inputs.map((input) => input.seq), [1, 2, 3]);
  let last = bundles[2];
  for (let seq = 4; seq <= 8; seq += 1) last = published(sender.push(key(seq, 'down'), seq * 10));
  assert.equal(last.inputs.length, MAX_REDUNDANT_COPIES + 1);
  assert.equal(last.first, 5);
});

test('a same-instant burst is paced into one bundle that carries every queued input', () => {
  const sender = new RedundantInputSender(7, 's1');
  published(sender.push(key(1, 'down'), 0));
  assert.deepEqual(sender.push(key(2, 'down'), 0), { kind: 'flushAfter', waitMs: MIN_BUNDLE_INTERVAL_MS });
  for (let seq = 3; seq <= 12; seq += 1) assert.deepEqual(sender.push(key(seq, 'down'), 0), { kind: 'queued' });
  const flushed = sender.flush(MIN_BUNDLE_INTERVAL_MS)!;
  assert.deepEqual(flushed.inputs.map((input) => input.seq), [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
  assert.equal(sender.flush(MIN_BUNDLE_INTERVAL_MS), null);
});

test('age and size bounds drop old or oversized redundancy but always send the new input', () => {
  const sender = new RedundantInputSender(7, 's1');
  published(sender.push(key(1, 'down'), 0));
  const late = published(sender.push(key(2, 'up'), MAX_REDUNDANT_AGE_MS + 1));
  assert.deepEqual(late.inputs.map((input) => input.seq), [2]);

  const big = new RedundantInputSender(7, 's2');
  const text = (seq: number) =>
    ({ ...key(seq, 'down'), kind: 'text', text: 'x'.repeat(MAX_BUNDLE_PAYLOAD_BYTES) }) as unknown as RemoteControlMessage;
  published(big.push(text(1), 0));
  const alone = published(big.push(text(2), MIN_BUNDLE_INTERVAL_MS));
  assert.deepEqual(alone.inputs.map((input) => input.seq), [2]);
  assert.equal(alone.first, 2);
});

test('bundles go out twice, then a trailing copy resends the burst\'s last bundle unless superseded', async () => {
  const clock = fakeClock();
  const sender = new RedundantInputSender(7, 's1');
  const sent: RemoteControlInputBundle[] = [];
  const publish = (bundle: RemoteControlInputBundle) => {
    sent.push(bundle);
    return Promise.resolve();
  };
  await publishRedundantInput(sender, key(1, 'down'), publish, clock);
  assert.equal(sent.length, IMMEDIATE_COPIES);
  clock.advance(10);
  await publishRedundantInput(sender, key(2, 'up'), publish, clock);
  assert.equal(sent.length, 2 * IMMEDIATE_COPIES);
  clock.advance(TRAILING_COPY_DELAYS_MS[0]);
  // Only the copy for the burst's last bundle fires; the first one was superseded.
  assert.equal(sent.length, 2 * IMMEDIATE_COPIES + 1);
  assert.deepEqual(sent.at(-1)!.inputs.map((input) => input.seq), [1, 2]);
  clock.advance(TRAILING_COPY_DELAYS_MS[1]);
  assert.equal(sent.length, 2 * IMMEDIATE_COPIES + 2);
  clock.advance(1_000);
  assert.equal(sent.length, 2 * IMMEDIATE_COPIES + 2, 'no copies beyond the configured trailing delays');

  // A burst inside the pacing interval publishes once, after the interval.
  await publishRedundantInput(sender, key(3, 'down'), publish, clock);
  const before = sent.length;
  await publishRedundantInput(sender, key(4, 'up'), publish, clock);
  await publishRedundantInput(sender, key(5, 'down'), publish, clock);
  assert.equal(sent.length, before);
  clock.advance(MIN_BUNDLE_INTERVAL_MS);
  assert.equal(sent.length, before + IMMEDIATE_COPIES);
  assert.deepEqual(sent.at(-1)!.inputs.map((input) => input.seq).slice(-2), [4, 5]);
});

test('hover moves stay off the redundant stream; drags, clicks, wheel, key, and text ride it', () => {
  const pointer = (action: string, buttons: number) =>
    ({ ...key(1, 'down'), kind: 'pointer', action, buttons, button: 0, x: 0.5, y: 0.5 }) as unknown as RemoteControlMessage;
  assert.equal(ridesRedundantStream(pointer('move', 0)), false);
  assert.equal(ridesRedundantStream(pointer('move', 1)), true);
  assert.equal(ridesRedundantStream(pointer('down', 1)), true);
  assert.equal(ridesRedundantStream(pointer('click', 0)), true);
  assert.equal(ridesRedundantStream({ ...key(1, 'down'), kind: 'wheel' } as unknown as RemoteControlMessage), true);
  assert.equal(ridesRedundantStream(key(1, 'down')), true);
  assert.equal(ridesRedundantStream({ ...key(1, 'down'), kind: 'release' } as unknown as RemoteControlMessage), false);
});

function uiContext(published: Array<{ data: Uint8Array; options: { reliable: boolean } }>): HarnessContext {
  return {
    state: {
      room: {
        localParticipant: {
          identity: 'web',
          publishData: (data: Uint8Array, options: { reliable: boolean }) => {
            published.push({ data, options });
            return Promise.resolve();
          }
        },
        remoteParticipants: new Map()
      },
      remoteControlSeq: 0,
      activeRemoteControl: {
        tileId: 'tile-1',
        targetUserId: 'native-host',
        windowId: 7,
        pointerId: null,
        grantToken: 'tok'
      }
    },
    ui: { logEvent: () => {}, showToast: () => {} },
    hook: {},
    cb: { shareTileForWindowId: () => null }
  } as unknown as HarnessContext;
}

function activeStatus(extra: Record<string, unknown>): Uint8Array {
  return new TextEncoder().encode(
    JSON.stringify({
      v: 1,
      kind: 'status',
      windowId: 7,
      targetUserId: 'web',
      controllerId: 'native-host',
      seq: 1,
      status: 'active',
      message: 'Remote control active for shared window',
      grantToken: 'tok',
      ...extra
    })
  );
}

test('the controller sends bundles only after the host advertises supportsRedundantInput', async () => {
  const priorDocument = (globalThis as { document?: unknown }).document;
  (globalThis as { document?: unknown }).document = { querySelectorAll: () => [], getElementById: () => null };
  try {
    const published: Array<{ data: Uint8Array; options: { reliable: boolean } }> = [];
    const ctx = uiContext(published);
    const ui = setupRemoteControlUi(ctx);

    // Old host: no capability, so the legacy reliable ordered packet.
    ui.handleRemoteControlPayload(activeStatus({}), 'native-host');
    await ui.publishRemoteControl(key(1, 'down'));
    assert.equal(published.at(-1)!.options.reliable, true);
    assert.equal(JSON.parse(new TextDecoder().decode(published.at(-1)!.data)).kind, 'key');

    ui.handleRemoteControlPayload(activeStatus({ supportsRedundantInput: true }), 'native-host');
    await ui.publishRemoteControl(key(2, 'up'));
    const bundle = JSON.parse(new TextDecoder().decode(published.at(-1)!.data));
    assert.equal(published.at(-1)!.options.reliable, false);
    assert.equal(bundle.kind, INPUT_BUNDLE_KIND);
    assert.equal(bundle.first, 1);
    assert.deepEqual(bundle.inputs.map((input: RemoteControlMessage) => input.seq), [2]);

    // A repeated active for the same grant keeps the stream going.
    const stream = bundle.stream;
    ui.handleRemoteControlPayload(activeStatus({ supportsRedundantInput: true }), 'native-host');
    await new Promise((resolve) => setTimeout(resolve, MIN_BUNDLE_INTERVAL_MS + 5));
    await ui.publishRemoteControl(key(3, 'down'));
    const next = JSON.parse(new TextDecoder().decode(published.at(-1)!.data));
    assert.equal(next.stream, stream);
    assert.deepEqual(next.inputs.map((input: RemoteControlMessage) => input.seq), [2, 3]);

    // A downgraded host (no flag) returns the controller to the legacy path.
    ui.handleRemoteControlPayload(activeStatus({}), 'native-host');
    await ui.publishRemoteControl(key(4, 'up'));
    assert.equal(published.at(-1)!.options.reliable, true);
  } finally {
    (globalThis as { document?: unknown }).document = priorDocument;
  }
});

test('a bundle rides the reliable channel while livekit reports the lossy buffer full', async () => {
  const priorDocument = (globalThis as { document?: unknown }).document;
  (globalThis as { document?: unknown }).document = { querySelectorAll: () => [], getElementById: () => null };
  try {
    const published: Array<{ data: Uint8Array; options: { reliable: boolean } }> = [];
    const ctx = uiContext(published);
    const listeners: Array<(isLow: boolean, kind: number) => void> = [];
    (ctx.state.room as unknown as { on: unknown }).on = (event: string, listener: (isLow: boolean, kind: number) => void) => {
      if (event === 'dcBufferStatusChanged') listeners.push(listener);
    };
    const ui = setupRemoteControlUi(ctx);
    ui.handleRemoteControlPayload(activeStatus({ supportsRedundantInput: true }), 'native-host');
    const settle = () => new Promise((resolve) => setTimeout(resolve, MIN_BUNDLE_INTERVAL_MS + 5));

    await ui.publishRemoteControl(key(1, 'down'));
    assert.equal(published.at(-1)!.options.reliable, false, 'lossy while the buffer is low');

    // livekit-client would now drop every lossy send (all copies at once).
    for (const listener of listeners) listener(false, 1);
    await settle();
    await ui.publishRemoteControl(key(2, 'up'));
    const congested = published.at(-1)!;
    assert.equal(congested.options.reliable, true, 'a full lossy buffer must not swallow the bundle');
    assert.equal(JSON.parse(new TextDecoder().decode(congested.data)).kind, INPUT_BUNDLE_KIND);

    // The reliable channel's status never redirects bundles.
    for (const listener of listeners) listener(true, 0);
    for (const listener of listeners) listener(true, 1);
    await settle();
    await ui.publishRemoteControl(key(3, 'down'));
    assert.equal(published.at(-1)!.options.reliable, false, 'back to lossy once the buffer drained');
  } finally {
    (globalThis as { document?: unknown }).document = priorDocument;
  }
});
