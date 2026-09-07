import assert from 'node:assert/strict';
import { test } from 'node:test';

import { parseCockpitCommandMessage, setupCockpit } from '../src/cockpit.ts';
import type { HarnessContext } from '../src/context.ts';
import { COCKPIT_TOPIC, type CockpitCommandMessage, type CockpitReportMessage } from '../src/trackNames.ts';

// ---------------------------------------------------------------------------
// #41: the native test-cockpit engine asks an unattended web peer to leave the
// room gracefully (`room.disconnect()`) BEFORE it kills headless Chrome, so
// the SFU drops the peer's publications at once instead of holding a ghost
// share tile for the ~25 s participant timeout into the next scenario. These
// cover the peer side of that contract: which `petal.cockpit` payloads make it
// disconnect, which it must ignore, and the acknowledgement it sends first.
// ---------------------------------------------------------------------------

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function disconnectContext(localIdentity = 'web-abc'): {
  ctx: HarnessContext;
  published: CockpitReportMessage[];
  disconnects: () => number;
} {
  const published: CockpitReportMessage[] = [];
  let disconnectCalls = 0;
  const publishData = async (data: Uint8Array, publishOptions: { topic?: string }) => {
    assert.equal(publishOptions.topic, COCKPIT_TOPIC);
    published.push(JSON.parse(decoder.decode(data)) as CockpitReportMessage);
  };
  const state = {
    room: {
      localParticipant: { identity: localIdentity, publishData },
      remoteParticipants: new Map<string, unknown>(),
      disconnect: async () => {
        disconnectCalls += 1;
      },
    },
  };
  const ctx = {
    state,
    hook: {},
    cb: {
      connectToMeeting: async () => {},
      resolveIdentity: () => localIdentity,
      startTestPatternShare: async () => {},
    },
  } as unknown as HarnessContext;
  return { ctx, published, disconnects: () => disconnectCalls };
}

function commandBytes(overrides: Partial<CockpitCommandMessage> = {}): Uint8Array {
  const message: CockpitCommandMessage = {
    v: 1,
    kind: 'command',
    command: 'disconnect',
    sentAtMs: 1_700_000_000_000,
    ...overrides,
  };
  return encoder.encode(JSON.stringify(message));
}

test('disconnect command addressed to this peer acknowledges over the topic, then disconnects the room', async () => {
  const { ctx, published, disconnects } = disconnectContext('web-abc');
  const cockpit = setupCockpit(ctx, () => 0);

  const acted = await cockpit.handleCockpitCommand(commandBytes({ target: 'web-abc' }), 'p-cockpit-1234');

  assert.equal(acted, true);
  assert.equal(disconnects(), 1);
  assert.equal(published.length, 1);
  const ack = published[0];
  assert.equal(ack.step, 'disconnect');
  assert.equal(ack.ok, true);
  assert.equal(ack.reporterId, 'web-abc');
  assert.match(ack.detail, /requested by p-cockpit-1234/);
});

test('an untargeted disconnect command is honoured too', async () => {
  const { ctx, disconnects } = disconnectContext('web-abc');
  const cockpit = setupCockpit(ctx, () => 0);

  assert.equal(await cockpit.handleCockpitCommand(commandBytes(), 'p-cockpit-1234'), true);
  assert.equal(disconnects(), 1);
});

test('a disconnect command addressed to a DIFFERENT peer is ignored', async () => {
  const { ctx, published, disconnects } = disconnectContext('web-abc');
  const cockpit = setupCockpit(ctx, () => 0);

  const acted = await cockpit.handleCockpitCommand(commandBytes({ target: 'web-other' }), 'p-cockpit-1234');

  assert.equal(acted, false);
  assert.equal(disconnects(), 0);
  assert.deepEqual(published, []);
});

test("another peer's report on the same topic never disconnects this peer", async () => {
  const { ctx, disconnects } = disconnectContext('web-abc');
  const cockpit = setupCockpit(ctx, () => 0);
  const report: CockpitReportMessage = {
    v: 1,
    reporterId: 'web-other',
    scenarioId: 'MULTI-3',
    step: 'done',
    ok: true,
    detail: 'peer report',
    sentAtMs: 1,
  };

  const acted = await cockpit.handleCockpitCommand(encoder.encode(JSON.stringify(report)), 'web-other');

  assert.equal(acted, false);
  assert.equal(disconnects(), 0);
});

test('malformed bytes and non-disconnect commands are ignored without throwing', async () => {
  const { ctx, disconnects } = disconnectContext('web-abc');
  const cockpit = setupCockpit(ctx, () => 0);

  assert.equal(await cockpit.handleCockpitCommand(encoder.encode('not json'), 'p-cockpit-1234'), false);
  assert.equal(
    await cockpit.handleCockpitCommand(
      encoder.encode(JSON.stringify({ v: 1, kind: 'command', command: 'reboot', sentAtMs: 1 })),
      'p-cockpit-1234'
    ),
    false
  );
  assert.equal(disconnects(), 0);
});

test('disconnect command with no active room is a no-op', async () => {
  const { ctx, disconnects } = disconnectContext('web-abc');
  (ctx.state as unknown as { room: unknown }).room = null;
  const cockpit = setupCockpit(ctx, () => 0);

  assert.equal(await cockpit.handleCockpitCommand(commandBytes(), 'p-cockpit-1234'), false);
  assert.equal(disconnects(), 0);
});

test('the hook exposes disconnect as a plain callable for a CDP-driven caller', async () => {
  const { ctx, disconnects } = disconnectContext('web-abc');
  setupCockpit(ctx, () => 0);

  await ctx.hook.cockpitAutoScenario!.disconnect();

  assert.equal(disconnects(), 1);
});

test('parseCockpitCommandMessage accepts only the v1 disconnect command shape', () => {
  assert.deepEqual(parseCockpitCommandMessage({ v: 1, kind: 'command', command: 'disconnect', sentAtMs: 5 }), {
    v: 1,
    kind: 'command',
    command: 'disconnect',
    target: undefined,
    sentAtMs: 5,
  });
  assert.equal(parseCockpitCommandMessage({ v: 2, kind: 'command', command: 'disconnect', sentAtMs: 5 }), null);
  assert.equal(parseCockpitCommandMessage({ v: 1, kind: 'report', command: 'disconnect', sentAtMs: 5 }), null);
  assert.equal(parseCockpitCommandMessage({ v: 1, kind: 'command', command: 'disconnect' }), null);
  assert.equal(parseCockpitCommandMessage({ v: 1, kind: 'command', command: 'disconnect', target: 7, sentAtMs: 5 }), null);
  assert.equal(parseCockpitCommandMessage(null), null);
  assert.equal(parseCockpitCommandMessage('disconnect'), null);
});
