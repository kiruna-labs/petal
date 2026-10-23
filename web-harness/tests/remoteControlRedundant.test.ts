import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  INPUT_BUNDLE_KIND,
  INPUT_BUNDLE_VERSION,
  MAX_BUNDLE_INPUTS,
  MAX_BUNDLE_PAYLOAD_BYTES,
  MAX_REDUNDANT_AGE_MS,
  TRAILING_COPY_DELAYS_MS,
  RedundantInputSender,
  publishRedundantInput,
  ridesRedundantStream,
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
  maxInputs: number;
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
  assert.equal(contract.maxInputs, MAX_BUNDLE_INPUTS);
  assert.equal(contract.maxPayloadBytes, MAX_BUNDLE_PAYLOAD_BYTES);
  assert.equal(contract.maxRedundantAgeMs, MAX_REDUNDANT_AGE_MS);
  assert.deepEqual(contract.trailingCopyDelaysMs, [...TRAILING_COPY_DELAYS_MS]);
  assert.deepEqual(Object.keys(contract.example).sort(), contract.fields);

  const sender = new RedundantInputSender(42, contract.example.stream);
  for (let i = 1; i < contract.example.first; i += 1) sender.push(key(i, 'down'), 0);
  sender.push(contract.example.inputs[0], MAX_REDUNDANT_AGE_MS + 1);
  const bundle = sender.push(contract.example.inputs[1], MAX_REDUNDANT_AGE_MS + 1);
  assert.deepEqual(bundle, contract.example);
});

test('each bundle carries the newest input plus the ones before it, so one loss is recovered', () => {
  const sender = new RedundantInputSender(7, 's1');
  const bundles = [1, 2, 3].map((seq) => sender.push(key(seq, 'down'), seq * 10));
  // If bundles[1] is lost, bundles[2] still carries seq 2.
  assert.equal(bundles[2].first, 1);
  assert.deepEqual(bundles[2].inputs.map((input) => input.seq), [1, 2, 3]);
  for (let seq = 4; seq <= 8; seq += 1) sender.push(key(seq, 'down'), 50);
  const capped = sender.bundle(50)!;
  assert.equal(capped.inputs.length, MAX_BUNDLE_INPUTS);
  assert.equal(capped.first, 5);
});

test('age and size bounds drop old or oversized redundancy but always send the newest input', () => {
  const sender = new RedundantInputSender(7, 's1');
  sender.push(key(1, 'down'), 0);
  const late = sender.push(key(2, 'up'), MAX_REDUNDANT_AGE_MS + 1);
  assert.deepEqual(late.inputs.map((input) => input.seq), [2]);

  const big = new RedundantInputSender(7, 's2');
  const text = (seq: number) =>
    ({ ...key(seq, 'down'), kind: 'text', text: 'x'.repeat(MAX_BUNDLE_PAYLOAD_BYTES) }) as unknown as RemoteControlMessage;
  big.push(text(1), 0);
  const alone = big.push(text(2), 0);
  assert.deepEqual(alone.inputs.map((input) => input.seq), [2]);
  assert.equal(alone.first, 2);
});

test('a trailing copy resends the final input of a burst and a newer input supersedes it', async () => {
  const clock = fakeClock();
  const sender = new RedundantInputSender(7, 's1');
  const sent: RemoteControlInputBundle[] = [];
  const publish = (bundle: RemoteControlInputBundle) => {
    sent.push(bundle);
    return Promise.resolve();
  };
  await publishRedundantInput(sender, key(1, 'down'), publish, clock);
  clock.advance(10);
  await publishRedundantInput(sender, key(2, 'up'), publish, clock);
  assert.equal(sent.length, 2);
  clock.advance(TRAILING_COPY_DELAYS_MS[0]);
  // Only the copy for the burst's last input (rseq 2) fires; rseq 1's was superseded.
  assert.equal(sent.length, 3);
  assert.deepEqual(sent[2].inputs.map((input) => input.seq), [1, 2]);
  clock.advance(TRAILING_COPY_DELAYS_MS[1]);
  assert.equal(sent.length, 4);
  assert.deepEqual(sent[3], sent[2]);
  clock.advance(1_000);
  assert.equal(sent.length, 4, 'no copies beyond the configured trailing delays');
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
