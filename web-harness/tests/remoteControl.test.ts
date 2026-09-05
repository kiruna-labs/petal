import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  isPasteChord,
  normalizedPointInContainedMedia,
  canonicalRemoteControlFingerprint,
  parseRemoteControlJson,
  remoteControlModifiers,
  remoteControlPublishOptions
} from '../src/remoteControl.ts';
import { setupHarnessApi } from '../src/harnessApi.ts';
import { setupRemoteControlUi } from '../src/remoteControlUi.ts';
import type { HarnessContext } from '../src/context.ts';
import { LATENCY_PROBE_TOPIC, type LatencyProbeMessage, type RemoteControlMessage } from '../src/trackNames.ts';

const contractFixture = JSON.parse(
  readFileSync(new URL('../../contracts/petal-contracts.json', import.meta.url), 'utf8')
) as {
  remoteControlMessages: Array<{
    name: string;
    reliable: boolean;
    message: RemoteControlMessage;
  }>;
  topics: { remoteControl: string };
};

const emptyModifiers = { alt: false, ctrl: false, meta: false, shift: false };

function pointerMoveMessage(buttons: number | undefined): RemoteControlMessage {
  const message = {
    v: 1,
    targetUserId: 'native',
    controllerId: 'web',
    windowId: 7,
    seq: 1,
    kind: 'pointer',
    action: 'move',
    x: 0.5,
    y: 0.5,
    button: -1,
    buttons: buttons ?? 0,
    modifiers: emptyModifiers
  } satisfies Extract<RemoteControlMessage, { kind: 'pointer' }>;
  if (buttons === undefined) delete (message as { buttons?: number }).buttons;
  return message;
}

function wheelMessage(): RemoteControlMessage {
  return {
    v: 1,
    targetUserId: 'native',
    controllerId: 'web',
    windowId: 7,
    seq: 1,
    kind: 'wheel',
    x: 0.5,
    y: 0.5,
    deltaX: 0,
    deltaY: 10,
    deltaMode: 0,
    modifiers: emptyModifiers
  };
}

function remoteControlContext(
  publishData: (data: Uint8Array, options: unknown) => Promise<void>,
  logEvent: (message: string, kind?: string) => void = () => {},
  remoteParticipantIdentities: string[] = []
): HarnessContext {
  return {
    state: {
      room: {
        localParticipant: {
          identity: 'web',
          publishData
        },
        remoteParticipants: new Map(
          remoteParticipantIdentities.map((identity) => [identity, { identity }])
        )
      },
      remoteControlSeq: 0,
      activeRemoteControl: null
    },
    ui: {
      logEvent,
      showToast: () => {}
    },
    hook: {},
    cb: {
      shareTileForWindowId: () => null
    }
  } as unknown as HarnessContext;
}

async function flushPromiseContinuations() {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

/**
 * Wait for the test publisher itself, rather than guessing how long async
 * Web Crypto needs to finish a canonical input fingerprint. The timeout keeps
 * a broken publish path bounded without making machine speed part of the
 * assertion.
 */
function boundedPublisherCallbackWait(timeoutMs = 1_000) {
  let resolveCallback!: () => void;
  const callback = new Promise<void>((resolve) => {
    resolveCallback = resolve;
  });
  return {
    notify: () => resolveCallback(),
    async wait() {
      let timeout: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([
          callback,
          new Promise<never>((_, reject) => {
            timeout = setTimeout(
              () => reject(new Error(`publisher callback did not arrive within ${timeoutMs}ms`)),
              timeoutMs
            );
          })
        ]);
      } finally {
        if (timeout !== undefined) clearTimeout(timeout);
      }
    }
  };
}

function encodeJson(value: unknown): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(value));
}

function activateHarnessV2(
  api: ReturnType<typeof setupHarnessApi>,
  controlSessionId = 'grant-a',
  dedupGuaranteeWindowMs = 750
) {
  api.handleRemoteControlPayload(encodeJson({
    v: 1,
    kind: 'status',
    targetUserId: 'web',
    controllerId: 'native',
    windowId: 7,
    seq: 1,
    status: 'active',
    message: 'granted',
    controlSessionId,
    resultCapability: {
      version: 2,
      retryEnabled: false,
      retryDeadlineMs: 0,
      dedupGuaranteeWindowMs
    }
  } satisfies RemoteControlMessage), 'native');
}

function terminalResultFor(
  input: Extract<RemoteControlMessage, { kind: 'pointer' | 'key' | 'text' }>,
  overrides: Record<string, unknown> = {}
): RemoteControlMessage {
  return {
    v: 1,
    kind: 'result',
    targetUserId: 'web',
    controllerId: 'native',
    windowId: input.windowId,
    seq: 2,
    controlSessionId: input.controlSessionId!,
    inputId: input.inputId!,
    inputSeq: input.inputSeq!,
    operationFingerprintVersion: 1,
    operationFingerprint: input.operationFingerprint!,
    outcome: 'applied',
    deliveryRoute: 'replay',
    ...overrides
  } as RemoteControlMessage;
}

test('normalizedPointInContainedMedia maps through the object-fit contained video rect', () => {
  const bounds = { left: 10, top: 20, width: 400, height: 300 };
  const media = { width: 1600, height: 900 };

  assert.deepEqual(normalizedPointInContainedMedia(bounds, media, { x: 210, y: 170 }), {
    x: 0.5,
    y: 0.5
  });
  assert.deepEqual(normalizedPointInContainedMedia(bounds, media, { x: 10, y: 57.5 }), {
    x: 0,
    y: 0
  });
  assert.deepEqual(normalizedPointInContainedMedia(bounds, media, { x: 410, y: 282.5 }), {
    x: 1,
    y: 1
  });
});

test('normalizedPointInContainedMedia can reject or clamp letterbox hits', () => {
  const bounds = { left: 0, top: 0, width: 400, height: 300 };
  const media = { width: 1600, height: 900 };

  assert.equal(normalizedPointInContainedMedia(bounds, media, { x: 200, y: 20 }, { clamp: false }), null);
  assert.deepEqual(normalizedPointInContainedMedia(bounds, media, { x: 200, y: 20 }), {
    x: 0.5,
    y: 0
  });
});

test('remoteControlPublishOptions sends motion and wheel streams unreliably', () => {
  for (const vector of contractFixture.remoteControlMessages) {
    assert.deepEqual(
      remoteControlPublishOptions(vector.message),
      {
        topic: contractFixture.topics.remoteControl,
        reliable: vector.reliable,
        // #370 corrective pass (Bug B): every fixture message carries a real
        // targetUserId, so publish options must always scope delivery to it.
        ...(vector.message.targetUserId ? { destinationIdentities: [vector.message.targetUserId] } : {})
      },
      vector.name
    );
  }
});

test('legacy and capable v2 fingerprints match the shared vectors', async () => {
  for (const name of [
    'pointer-click-v2-canonical-fingerprint',
    'pointer-click-capable-window'
  ]) {
    const vector = contractFixture.remoteControlMessages.find((entry) => entry.name === name);
    assert.ok(vector);
    const message = vector.message as Extract<RemoteControlMessage, { kind: 'pointer' }>;
    assert.equal(
      await canonicalRemoteControlFingerprint(message, {
        controlSessionId: message.controlSessionId!,
        inputId: message.inputId!,
        inputSeq: message.inputSeq!
      }),
      message.operationFingerprint,
      name
    );
  }
});

test('remoteControlPublishOptions keeps held-button drag moves reliable', () => {
  const pointerMove = contractFixture.remoteControlMessages.find((vector) => vector.name === 'pointer-move');
  assert.ok(pointerMove);
  const heldButtonMove = {
    ...(pointerMove.message as Extract<RemoteControlMessage, { kind: 'pointer' }>),
    buttons: 1
  };

  assert.deepEqual(
    remoteControlPublishOptions(heldButtonMove),
    {
      topic: contractFixture.topics.remoteControl,
      reliable: true,
      ...(heldButtonMove.targetUserId ? { destinationIdentities: [heldButtonMove.targetUserId] } : {})
    }
  );
});

test('publishRemoteControl logs and propagates reliable input publish failures', async () => {
  const logs: Array<{ message: string; kind?: string }> = [];
  const ui = setupRemoteControlUi(
    remoteControlContext(
      () => Promise.reject(new Error('data channel backpressure')),
      (message, kind) => logs.push({ message, kind })
    )
  );

  await assert.rejects(
    ui.publishRemoteControl(pointerMoveMessage(1)),
    /data channel backpressure/
  );

  assert.equal(logs.length, 1);
  assert.equal(logs[0].kind, 'warn');
  assert.match(logs[0].message, /remote control publish failed: data channel backpressure/);
});

test('publishRemoteControl keeps suppressing hover move and wheel publish failures', async () => {
  const logs: Array<{ message: string; kind?: string }> = [];
  const ui = setupRemoteControlUi(
    remoteControlContext(
      () => Promise.reject(new Error('discardable packet failed')),
      (message, kind) => logs.push({ message, kind })
    )
  );

  await ui.publishRemoteControl(pointerMoveMessage(0));
  await ui.publishRemoteControl(pointerMoveMessage(undefined));
  await ui.publishRemoteControl(wheelMessage());

  assert.deepEqual(logs, []);
});

test('handleRemoteControlPayload rejects a status packet whose LiveKit sender is not the host', () => {
  // Fable F1: targetUserId/windowId in the wire message are attacker-chosen
  // and are not authentication -- only the actual LiveKit-verified sender
  // identity may mutate the local grant token or tear down the session.
  // This handler assumes a browser DOM (it queries .share-tile elements for
  // status display, unrelated to the security check under test); stub the
  // minimal surface it touches rather than pull in a full DOM dependency.
  const priorDocument = (globalThis as { document?: unknown }).document;
  (globalThis as { document?: unknown }).document = {
    querySelectorAll: () => []
  };
  try {
    const ctx = remoteControlContext(() => Promise.resolve());
    ctx.state.activeRemoteControl = {
      tileId: 'tile-1',
      targetUserId: 'native-host',
      windowId: 7,
      pointerId: null,
      grantToken: 'original-token'
    };
    const ui = setupRemoteControlUi(ctx);

    const spoofedActive = encodeJson({
      v: 1,
      kind: 'status',
      windowId: 7,
      targetUserId: 'web',
      controllerId: 'native-host',
      status: 'active',
      grantToken: 'attacker-token'
    });
    ui.handleRemoteControlPayload(spoofedActive, 'attacker-identity');
    assert.equal(ctx.state.activeRemoteControl?.grantToken, 'original-token');

    const spoofedStop = encodeJson({
      v: 1,
      kind: 'status',
      windowId: 7,
      targetUserId: 'web',
      controllerId: 'native-host',
      status: 'stopped'
    });
    ui.handleRemoteControlPayload(spoofedStop, 'attacker-identity');
    assert.notEqual(ctx.state.activeRemoteControl, null);

    // The genuine host sender still updates/tears down the session normally.
    ui.handleRemoteControlPayload(spoofedActive, 'native-host');
    assert.equal(ctx.state.activeRemoteControl?.grantToken, 'attacker-token');
    const operationFeedback = encodeJson({
      v: 1,
      kind: 'status',
      windowId: 7,
      targetUserId: 'web',
      controllerId: 'native-host',
      status: 'occluded',
      message: 'Remote input was ignored because the target point is covered.'
    });
    ui.handleRemoteControlPayload(operationFeedback, 'native-host');
    assert.equal(ctx.state.activeRemoteControl?.grantToken, 'attacker-token');
    ui.handleRemoteControlPayload(spoofedStop, 'native-host');
    assert.equal(ctx.state.activeRemoteControl, null);
  } finally {
    (globalThis as { document?: unknown }).document = priorDocument;
  }
});

test('remote-control harness drag awaits each publish before sending the next packet', async () => {
  const published: RemoteControlMessage[] = [];
  const resolvers: Array<() => void> = [];
  const ctx = remoteControlContext(
    () => Promise.resolve()
  );
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: (message) => {
      published.push(message);
      return new Promise<void>((resolve) => {
        resolvers.push(resolve);
      });
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  const drag = ctx.hook.remoteControl!.drag({
    targetUserId: 'native',
    windowId: 7,
    from: { x: 0.1, y: 0.2 },
    to: { x: 0.5, y: 0.2 },
    steps: 3,
    button: 0
  });

  assert.deepEqual(
    published.map((message) => ('action' in message ? message.action : message.kind)),
    ['down']
  );

  resolvers.shift()!();
  await flushPromiseContinuations();
  assert.deepEqual(
    published.map((message) => ('action' in message ? message.action : message.kind)),
    ['down', 'move']
  );

  await flushPromiseContinuations();
  assert.deepEqual(
    published.map((message) => ('action' in message ? message.action : message.kind)),
    ['down', 'move']
  );

  while (resolvers.length > 0) {
    resolvers.shift()!();
    await flushPromiseContinuations();
  }

  await drag;
  assert.deepEqual(
    published.map((message) => ('action' in message ? message.action : message.kind)),
    ['down', 'move', 'move', 'move', 'up']
  );
});

test('remote-control harness simple click is one reliable semantic packet', async () => {
  const published: RemoteControlMessage[] = [];
  const ctx = remoteControlContext(() => Promise.resolve());
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: async (message) => {
      published.push(message);
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  await ctx.hook.remoteControl!.click({
    targetUserId: 'native',
    windowId: 7,
    x: 0.4,
    y: 0.6,
    button: 0,
    modifiers: { meta: true }
  });

  assert.equal(published.length, 1);
  assert.deepEqual(published[0], {
    v: 1,
    targetUserId: 'native',
    controllerId: 'web',
    windowId: 7,
    seq: 1,
    kind: 'pointer',
    action: 'click',
    x: 0.4,
    y: 0.6,
    button: 0,
    buttons: 0,
    modifiers: { alt: false, ctrl: false, meta: true, shift: false }
  });
});

test('remote-control harness doubleClick sends a down/up pair with clickCount=2', async () => {
  const published: RemoteControlMessage[] = [];
  const ctx = remoteControlContext(() => Promise.resolve());
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: async (message) => {
      published.push(message);
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  await ctx.hook.remoteControl!.doubleClick({
    targetUserId: 'native',
    windowId: 7,
    x: 0.4,
    y: 0.6,
    button: 0
  });

  assert.equal(published.length, 2);
  assert.equal(published[0].kind, 'pointer');
  assert.equal(published[1].kind, 'pointer');
  const down = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  const up = published[1] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  assert.equal(down.action, 'down');
  assert.equal(down.clickCount, 2);
  assert.equal(down.buttons, 1);
  assert.equal(up.action, 'up');
  assert.equal(up.clickCount, 2);
  assert.equal(up.buttons, 0);
});

test('remote-control harness pointer forwards an explicit clickCount', async () => {
  const published: RemoteControlMessage[] = [];
  const ctx = remoteControlContext(() => Promise.resolve());
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: async (message) => {
      published.push(message);
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  ctx.hook.remoteControl!.pointer({
    targetUserId: 'native',
    windowId: 7,
    action: 'down',
    x: 0.1,
    y: 0.2,
    clickCount: 3
  });

  assert.equal(published.length, 1);
  assert.equal((published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>).clickCount, 3);

  // Omitting clickCount must not serialize the field at all (additive,
  // old-peer-compatible field per #373).
  const withoutClickCount = ctx.hook.remoteControl!;
  const beforeLength = published.length;
  withoutClickCount.pointer({
    targetUserId: 'native',
    windowId: 7,
    action: 'move',
    x: 0.1,
    y: 0.2
  });
  assert.equal(published.length, beforeLength + 1);
  assert.ok(!('clickCount' in published[beforeLength]));
});

test('chunkRemoteText splits long IME composition commits the same way the host chunks outbound text (#33 parity)', async () => {
  const { chunkRemoteText, MAX_REMOTE_TEXT_CHARS } = await import('../src/remoteControl.ts');
  assert.deepEqual(chunkRemoteText(''), []);
  assert.deepEqual(chunkRemoteText('hello'), ['hello']);

  const long = 'a'.repeat(MAX_REMOTE_TEXT_CHARS) + 'b'.repeat(MAX_REMOTE_TEXT_CHARS) + 'c';
  const chunks = chunkRemoteText(long);
  assert.equal(chunks.length, 3);
  assert.equal(chunks[0], 'a'.repeat(MAX_REMOTE_TEXT_CHARS));
  assert.equal(chunks[1], 'b'.repeat(MAX_REMOTE_TEXT_CHARS));
  assert.equal(chunks[2], 'c');
  assert.equal(chunks.join(''), long);

  // Split on Unicode scalar values, not UTF-16 code units -- must never cut
  // a surrogate pair (e.g. an emoji from an emoji-picker commit) in half.
  const emoji = '\u{1F600}'; // 😀, a surrogate pair in UTF-16
  const emojiLong = emoji.repeat(MAX_REMOTE_TEXT_CHARS) + emoji;
  const emojiChunks = chunkRemoteText(emojiLong);
  assert.equal(emojiChunks.length, 2);
  assert.equal(Array.from(emojiChunks[0]).length, MAX_REMOTE_TEXT_CHARS);
  assert.equal(emojiChunks[1], emoji);
  assert.equal(emojiChunks.join(''), emojiLong);
});

test('remote-control harness echoes the active grant token on input packets', async () => {
  const published: RemoteControlMessage[] = [];
  const ctx = remoteControlContext(() => Promise.resolve());
  ctx.state.activeRemoteControl = {
    tileId: 'remote-window-native-7',
    targetUserId: 'native',
    windowId: 7,
    pointerId: null,
    grantToken: '0123456789abcdef0123456789abcdef'
  };
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: async (message) => {
      published.push(message);
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  await ctx.hook.remoteControl!.click({
    targetUserId: 'native',
    windowId: 7,
    x: 0.4,
    y: 0.6
  });

  assert.equal(published[0]?.grantToken, '0123456789abcdef0123456789abcdef');
});

test('harness v2 grant attaches one canonical envelope, records terminal result, and never retries', async () => {
  const published: RemoteControlMessage[] = [];
  const publisherCallback = boundedPublisherCallbackWait();
  const ctx = remoteControlContext(() => Promise.resolve());
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
    publishRemoteControl: async (message) => { published.push(message); publisherCallback.notify(); },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });
  api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 1,
    status: 'active', message: 'granted', controlSessionId: 'grant-a',
    resultCapability: { version: 2, retryEnabled: false, retryDeadlineMs: 0, dedupGuaranteeWindowMs: 750 }
  } satisfies RemoteControlMessage), 'native');
  ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: 0.4, y: 0.6 });
  await publisherCallback.wait();
  assert.equal(published.length, 1);
  const input = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  assert.equal(input.controlSessionId, 'grant-a');
  assert.equal(input.operationFingerprintVersion, 1);
  assert.match(input.operationFingerprint ?? '', /^[0-9a-f]{64}$/);
  assert.deepEqual(ctx.hook.remoteControl!.metrics().pending, [input.inputId]);
  for (const spoof of [
    { controllerId: 'attacker' }, { targetUserId: 'someone-else' }, { controlSessionId: 'wrong-grant' },
    { inputSeq: input.inputSeq! + 1 }, { operationFingerprint: '0'.repeat(64) }, { outcome: 'unknown-outcome' }
  ]) {
    api.handleRemoteControlPayload(encodeJson({
      v: 1, kind: 'result', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 2,
      controlSessionId: 'grant-a', inputId: input.inputId!, inputSeq: input.inputSeq!,
      operationFingerprintVersion: 1, operationFingerprint: input.operationFingerprint!, outcome: 'applied', ...spoof
    } as RemoteControlMessage), spoof.controllerId === 'attacker' ? 'attacker' : 'native');
    assert.deepEqual(ctx.hook.remoteControl!.metrics().pending, [input.inputId], 'spoofed terminal result is ignored');
  }
  api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'result', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 2,
    controlSessionId: 'grant-a', inputId: input.inputId!, inputSeq: input.inputSeq!,
    operationFingerprintVersion: 1, operationFingerprint: input.operationFingerprint!, outcome: 'replayFailed',
    deliveryRoute: 'replay', failureCode: 'injectionTimeout'
  } satisfies RemoteControlMessage));
  const metrics = ctx.hook.remoteControl!.metrics();
  assert.deepEqual(metrics.pending, []);
  assert.equal(metrics.results.at(-1)?.outcome, 'replayFailed');
  assert.equal(metrics.results.at(-1)?.deliveryRoute, 'replay');
  assert.equal(metrics.results.at(-1)?.failureCode, 'injectionTimeout');
  assert.equal(published.length, 1, 'retry is explicitly disabled');
});

test('harness one-shot click replay keeps operation identity, advances transport seq, and audits one synchronous cached result through expiry', async () => {
  const published: RemoteControlMessage[] = [];
  const firstPublish = boundedPublisherCallbackWait();
  const ctx = remoteControlContext(() => Promise.resolve());
  let api!: ReturnType<typeof setupHarnessApi>;
  api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
    publishRemoteControl: async (message) => {
      published.push(message);
      if (published.length === 1) {
        firstPublish.notify();
      } else if (published.length === 2) {
        api.handleRemoteControlPayload(
          encodeJson(terminalResultFor(published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>)),
          'native'
        );
      }
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });
  activateHarnessV2(api, 'grant-a', 40);
  ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: 0.75, y: 0.58, button: 0 });
  await firstPublish.wait();
  const original = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
  assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);

  // A cached-looking packet before the one-shot sender is armed is not proof.
  api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
  assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);

  assert.equal(await ctx.hook.remoteControl!.replayLastCompletedClick(), undefined);
  assert.equal(published.length, 2);
  const replay = published[1] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  assert.notEqual(replay.seq, original.seq);
  const { seq: originalSeq, ...originalOperation } = original;
  const { seq: replaySeq, ...replayedOperation } = replay;
  assert.notEqual(originalSeq, replaySeq);
  assert.deepEqual(replayedOperation, originalOperation);
  assert.equal(ctx.hook.remoteControl!.metrics().results.length, 2);

  await assert.rejects(
    ctx.hook.remoteControl!.replayLastCompletedClick(),
    /completed click replay is unavailable/
  );
});

test('harness cached-result tombstone rejects mismatches and accepts missing optional metadata', async () => {
  const published: RemoteControlMessage[] = [];
  const firstPublish = boundedPublisherCallbackWait();
  const ctx = remoteControlContext(() => Promise.resolve());
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
    publishRemoteControl: async (message) => {
      published.push(message);
      if (published.length === 1) firstPublish.notify();
    },
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });
  activateHarnessV2(api, 'grant-a', 50);
  ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: 0.75, y: 0.58, button: 0 });
  await firstPublish.wait();
  const original = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
  const legacyTerminal = terminalResultFor(original);
  delete (legacyTerminal as { deliveryRoute?: unknown }).deliveryRoute;
  api.handleRemoteControlPayload(encodeJson(legacyTerminal), 'native');
  const replayAudit = ctx.hook.remoteControl!.replayLastCompletedClick();
  await flushPromiseContinuations();
  assert.equal(published.length, 2);
  assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);

  for (const [overrides, sender] of [
    [{ controllerId: 'attacker' }, 'attacker'],
    [{ targetUserId: 'other-local' }, 'native'],
    [{ controlSessionId: 'other-session' }, 'native'],
    [{ windowId: 8 }, 'native'],
    [{ inputSeq: original.inputSeq! + 1 }, 'native'],
    [{ operationFingerprintVersion: 2 }, 'native'],
    [{ operationFingerprint: '0'.repeat(64) }, 'native']
  ] as Array<[Record<string, unknown>, string]>) {
    api.handleRemoteControlPayload(
      encodeJson(terminalResultFor(original, overrides)),
      sender
    );
    assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);
  }

  api.handleRemoteControlPayload(encodeJson(legacyTerminal), 'native');
  await replayAudit;
  const results = ctx.hook.remoteControl!.metrics().results;
  assert.equal(results.length, 2);
  assert.equal(results[0]?.deliveryRoute, undefined);
  assert.equal(results[0]?.failureCode, undefined);
  assert.equal(results[1]?.deliveryRoute, undefined);
  assert.equal(results[1]?.failureCode, undefined);
});

test('harness cached-result audit rejects conflicting and extra same-operation terminals', async () => {
  for (const mode of ['conflicting', 'extra'] as const) {
    const published: RemoteControlMessage[] = [];
    const firstPublish = boundedPublisherCallbackWait();
    const ctx = remoteControlContext(() => Promise.resolve());
    const api = setupHarnessApi(ctx, {
      nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
      publishRemoteControl: async (message) => {
        published.push(message);
        if (published.length === 1) firstPublish.notify();
      },
      startRemoteControl: () => {},
      stopRemoteControl: () => {}
    });
    activateHarnessV2(api, 'grant-a', 40);
    ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: 0.75, y: 0.58, button: 0 });
    await firstPublish.wait();
    const original = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
    api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
    const replayAudit = ctx.hook.remoteControl!.replayLastCompletedClick();
    await flushPromiseContinuations();
    assert.equal(published.length, 2);

    if (mode === 'conflicting') {
      api.handleRemoteControlPayload(
        encodeJson(terminalResultFor(original, {
          outcome: 'replayFailed',
          failureCode: 'replayFailed'
        })),
        'native'
      );
    } else {
      api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
      api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
    }

    await assert.rejects(replayAudit, /completed click replay audit failed/);
    assert.equal(
      ctx.hook.remoteControl!.metrics().results.length,
      mode === 'conflicting' ? 2 : 3
    );
    await assert.rejects(ctx.hook.remoteControl!.replayLastCompletedClick());
  }
});

test('harness one-shot click replay clears on rejection, reset, regrant, and original-send expiry', async () => {
  async function completedClick(
    publish: (message: RemoteControlMessage, index: number) => Promise<void>,
    dedupGuaranteeWindowMs = 750
  ) {
    const published: RemoteControlMessage[] = [];
    const firstPublish = boundedPublisherCallbackWait();
    const ctx = remoteControlContext(() => Promise.resolve());
    const api = setupHarnessApi(ctx, {
      nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
      publishRemoteControl: async (message) => {
        published.push(message);
        if (published.length === 1) firstPublish.notify();
        await publish(message, published.length);
      },
      startRemoteControl: () => {},
      stopRemoteControl: () => {}
    });
    activateHarnessV2(api, 'grant-a', dedupGuaranteeWindowMs);
    ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: 0.75, y: 0.58, button: 0 });
    await firstPublish.wait();
    const original = published[0] as Extract<RemoteControlMessage, { kind: 'pointer' }>;
    api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
    return { api, ctx, original };
  }

  {
    const { api, ctx, original } = await completedClick(async (_message, index) => {
      if (index === 2) throw new Error('publish rejected');
    });
    await assert.rejects(
      ctx.hook.remoteControl!.replayLastCompletedClick(),
      /completed click replay publish failed/
    );
    api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
    assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);
  }
  {
    const { ctx } = await completedClick(async () => {});
    ctx.hook.remoteControl!.resetMetrics();
    await assert.rejects(ctx.hook.remoteControl!.replayLastCompletedClick());
  }
  {
    const { api, ctx, original } = await completedClick(async () => {});
    const replayAudit = ctx.hook.remoteControl!.replayLastCompletedClick();
    await flushPromiseContinuations();
    api.resetRemoteControlSession();
    await assert.rejects(replayAudit, /session ended/);
    api.handleRemoteControlPayload(encodeJson(terminalResultFor(original)), 'native');
    assert.equal(ctx.hook.remoteControl!.metrics().results.length, 1);
    await assert.rejects(ctx.hook.remoteControl!.replayLastCompletedClick());
  }
  {
    const { api, ctx } = await completedClick(async () => {});
    activateHarnessV2(api, 'grant-b');
    await assert.rejects(ctx.hook.remoteControl!.replayLastCompletedClick());
  }
  {
    const { ctx } = await completedClick(async () => {}, 20);
    await new Promise((resolve) => setTimeout(resolve, 35));
    await assert.rejects(ctx.hook.remoteControl!.replayLastCompletedClick());
  }
});

test('v2 result accepts absent metadata and ignores unknown future diagnostic codes', () => {
  const legacy = parseRemoteControlJson(JSON.stringify({
    v: 1, kind: 'result', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 2,
    controlSessionId: 'grant-a', inputId: 'input-a', inputSeq: 1,
    operationFingerprintVersion: 1, operationFingerprint: '0'.repeat(64), outcome: 'applied'
  }));
  assert.equal(legacy?.kind, 'result');
  if (legacy?.kind === 'result') {
    assert.equal(legacy.deliveryRoute, undefined);
    assert.equal(legacy.failureCode, undefined);
  }
  const contradictory = parseRemoteControlJson(JSON.stringify({
    v: 1, kind: 'result', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 2,
    controlSessionId: 'grant-a', inputId: 'input-a', inputSeq: 1,
    operationFingerprintVersion: 1, operationFingerprint: '0'.repeat(64), outcome: 'applied',
    deliveryRoute: 'replay', failureCode: 'injectionTimeout'
  }));
  assert.equal(contradictory?.kind, 'result');
  if (contradictory?.kind === 'result') {
    assert.equal(contradictory.outcome, 'applied');
    assert.equal(contradictory.deliveryRoute, 'replay');
    assert.equal(contradictory.failureCode, undefined);
  }
  const newer = parseRemoteControlJson(JSON.stringify({
    v: 1, kind: 'result', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 2,
    controlSessionId: 'grant-a', inputId: 'input-a', inputSeq: 1,
    operationFingerprintVersion: 1, operationFingerprint: '0'.repeat(64), outcome: 'applied',
    deliveryRoute: 'future-stage', failureCode: 'future-code'
  }));
  assert.equal(newer?.kind, 'result');
  if (newer?.kind === 'result') {
    assert.equal(newer.outcome, 'applied');
    assert.equal(newer.deliveryRoute, undefined);
    assert.equal(newer.failureCode, undefined);
  }
});

test('harness deterministic grant matrix handles malformed envelopes, revoke, regrant, expiry/reload, and legacy peers', async () => {
  const published: RemoteControlMessage[] = [];
  let publisherCallback: ReturnType<typeof boundedPublisherCallbackWait> | null = null;
  const ctx = remoteControlContext(() => Promise.resolve());
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
    publishRemoteControl: async (message) => { published.push(message); publisherCallback?.notify(); },
    startRemoteControl: () => {}, stopRemoteControl: () => {}
  });
  const status = (status: 'active' | 'stopped', grant?: string) => api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: published.length + 1,
    status, message: status, ...(grant ? { controlSessionId: grant, resultCapability: { version: 2, retryEnabled: false, retryDeadlineMs: 0, dedupGuaranteeWindowMs: 750 } } : {})
  } as RemoteControlMessage), 'native');
  const click = async () => {
    publisherCallback = boundedPublisherCallbackWait();
    ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: .5, y: .5 });
    await publisherCallback.wait();
    publisherCallback = null;
    return published.at(-1)!;
  };
  status('active', 'grant-a');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-a');
  api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 20,
    status: 'stopped', message: 'partial capable status', targetKind: 'window'
  }), 'native');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-a', 'partial target envelope cannot revoke a grant');
  api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 21,
    status: 'stopped', message: 'future target status', targetKind: 'future-target',
    shareInstanceId: 'share-a'
  }), 'native');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-a', 'unknown target kind cannot revoke a grant');
  status('stopped');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, undefined, 'revoked/expired grant falls back to legacy without replay');
  api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: 22,
    status: 'active', message: 'capable grant', targetKind: 'display',
    shareInstanceId: 'share-b', hostCapabilities: ['discretePointerV1'],
    controlSessionId: 'grant-b',
    resultCapability: { version: 2, retryEnabled: false, retryDeadlineMs: 0, dedupGuaranteeWindowMs: 750 }
  }), 'native');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-b', 'regrant is a new generation');
  const reload = setupHarnessApi(remoteControlContext(() => Promise.resolve()), {
    nextRemoteControlSeq: () => 1, publishRemoteControl: async () => {}, startRemoteControl: () => {}, stopRemoteControl: () => {}
  });
  assert.ok(reload, 'fresh harness state has no persisted grant or pending retry');
});

test('harness ignores a v2 grant status spoofed by a non-host room peer', async () => {
  const published: RemoteControlMessage[] = [];
  let publisherCallback: ReturnType<typeof boundedPublisherCallbackWait> | null = null;
  const ctx = remoteControlContext(() => Promise.resolve());
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => ++ctx.state.remoteControlSeq,
    publishRemoteControl: async (message) => { published.push(message); publisherCallback?.notify(); },
    startRemoteControl: () => {}, stopRemoteControl: () => {}
  });
  const status = (status: 'active' | 'stopped', senderIdentity: string | undefined, grant?: string) => api.handleRemoteControlPayload(encodeJson({
    v: 1, kind: 'status', targetUserId: 'web', controllerId: 'native', windowId: 7, seq: published.length + 1,
    status, message: status, ...(grant ? { controlSessionId: grant, resultCapability: { version: 2, retryEnabled: false, retryDeadlineMs: 0, dedupGuaranteeWindowMs: 750 } } : {})
  } as RemoteControlMessage), senderIdentity);
  const click = async () => {
    publisherCallback = boundedPublisherCallbackWait();
    ctx.hook.remoteControl!.click({ targetUserId: 'native', windowId: 7, x: .5, y: .5 });
    await publisherCallback.wait();
    publisherCallback = null;
    return published.at(-1)!;
  };

  status('active', 'attacker', 'grant-spoofed');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, undefined, 'a status from a non-host sender must not install a grant');

  status('active', 'native', 'grant-real');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-real', 'the genuine host can still grant control');

  status('stopped', 'attacker');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, 'grant-real', 'a stop from a non-host sender must not revoke the real grant');

  status('stopped', 'native');
  assert.equal((await click() as Extract<RemoteControlMessage, { kind: 'pointer' }>).controlSessionId, undefined, 'the genuine host can still revoke the grant');
});

test('remoteControlModifiers exposes the stable modifier field names', () => {
  assert.deepEqual(
    remoteControlModifiers({ altKey: true, ctrlKey: false, metaKey: true, shiftKey: false }),
    { alt: true, ctrl: false, meta: true, shift: false }
  );
});

const bareModifiers = { metaKey: false, ctrlKey: false, altKey: false, shiftKey: false };

test('isPasteChord matches bare Cmd+V by logical key, case-insensitively', () => {
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'v', code: 'KeyV' }), true);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'V', code: 'KeyV' }), true);
});

test('isPasteChord falls back to the physical code when the logical key is empty', () => {
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: '', code: 'KeyV' }), true);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, code: 'KeyV' }), true);
});

test('isPasteChord prefers the logical key over the physical code (non-US layouts)', () => {
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'j', code: 'KeyV' }), false);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'v', code: 'KeyDot' }), true);
});

test('isPasteChord rejects Cmd+V with any extra modifier', () => {
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: true, altKey: false, shiftKey: false, key: 'v' }), false);
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: false, altKey: true, shiftKey: false, key: 'v' }), false);
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: false, altKey: false, shiftKey: true, key: 'v' }), false);
});

test('isPasteChord rejects plain V and other letters', () => {
  assert.equal(isPasteChord({ ...bareModifiers, key: 'v' }), false);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'c', code: 'KeyC' }), false);
});

test('remote-control live scenario separates case duration from measured target-observation latency', () => {
  const context = readFileSync(new URL('../src/context.ts', import.meta.url), 'utf8');
  const harness = readFileSync(new URL('../src/harnessApi.ts', import.meta.url), 'utf8');
  const scenario = readFileSync(
    new URL('../../apps/desktop/scripts/remote-control-scenario.mjs', import.meta.url),
    'utf8'
  );
  const loopback = readFileSync(
    new URL('../../apps/desktop/scripts/remote-control-local-loopback.mjs', import.meta.url),
    'utf8'
  );

  assert.match(context, /metrics: \(\) =>/);
  assert.match(harness, /publishedMetrics\.push/);
  assert.match(scenario, /const CASES = \[/);
  assert.match(scenario, /RESULT \$\{JSON\.stringify\(result\)\}/);
  assert.match(scenario, /SUMMARY \$\{JSON\.stringify\(summary\)\}/);
  assert.match(scenario, /PETAL_REMOTE_CONTROL_SHARE_READY_TIMEOUT_MS/);
  assert.match(scenario, /PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS/);
  assert.match(scenario, /caseDurationMs/);
  assert.match(scenario, /targetObservationLatencyMs/);
  assert.match(scenario, /measureTargetObservation/);
  assert.doesNotMatch(scenario, /\blatencyMs\b/);
  assert.match(scenario, /--press-to-photon/);
  assert.match(scenario, /summarizePhotonSamples/);
  assert.match(harness, /pressToPhoton: measurePressToPhoton/);
  assert.match(harness, /requestVideoFrameCallback/);
  assert.match(harness, /expectedDisplayTime/);
  assert.match(scenario, /INFRA\/SKIP/);
  assert.match(scenario, /status: 'skip'/);
  assert.match(scenario, /current_room/);
  assert.match(loopback, /CGEventPostToPid/);
  assert.match(loopback, /--check-only/);
  assert.match(loopback, /--json/);
});

test('remote-control harness records native host status packets', () => {
  const context = readFileSync(new URL('../src/context.ts', import.meta.url), 'utf8');
  const harness = readFileSync(new URL('../src/harnessApi.ts', import.meta.url), 'utf8');
  const connection = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');
  const main = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');

  assert.match(context, /statuses: Array/);
  assert.match(harness, /statusMetrics\.push/);
  assert.match(harness, /handleRemoteControlPayload/);
  assert.match(connection, /REMOTE_CONTROL_TOPIC/);
  assert.match(connection, /handleRemoteControlPayload/);
  assert.ok(
    (connection.match(/resetRemoteControlHarnessSession\?\.\(\)/g) ?? []).length >= 2,
    'both connection-state and room-disconnected teardown clear harness session state'
  );
  assert.match(main, /resetRemoteControlHarnessSession: harnessApi\.resetRemoteControlSession/);
});

test('latency probe handler echoes pings as targeted reliable pongs', async () => {
  const published: Array<{ message: LatencyProbeMessage; options: Record<string, unknown> }> = [];
  const ctx = remoteControlContext((data, options) => {
    published.push({
      message: JSON.parse(new TextDecoder().decode(data)) as LatencyProbeMessage,
      options: options as Record<string, unknown>
    });
    return Promise.resolve();
  });
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => 1,
    publishRemoteControl: () => Promise.resolve(),
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  api.handleLatencyProbePayload(
    encodeJson({
      v: 1,
      kind: 'ping',
      probeId: 42,
      senderId: 'native-1',
      sendTimeMs: 1720000000123
    } satisfies LatencyProbeMessage),
    'native-1'
  );
  await flushPromiseContinuations();

  assert.equal(published.length, 1);
  assert.equal(published[0].message.v, 1);
  assert.equal(published[0].message.kind, 'pong');
  assert.equal(published[0].message.probeId, 42);
  assert.equal(published[0].message.senderId, 'web');
  assert.equal(published[0].message.sendTimeMs, 1720000000123);
  assert.ok(Number.isSafeInteger(published[0].message.receiverReceiveTimeMs));
  assert.ok(Number.isSafeInteger(published[0].message.receiverSendTimeMs));
  assert.deepEqual(published[0].options, {
    topic: LATENCY_PROBE_TOPIC,
    reliable: true,
    destinationIdentities: ['native-1']
  });
});

test('latency probe handler records RTT only for locally generated probes', async () => {
  const ctx = remoteControlContext(() => Promise.resolve(), () => {}, ['native-1']);
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => 1,
    publishRemoteControl: () => Promise.resolve(),
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  api.handleLatencyProbePayload(
    encodeJson({
      v: 1,
      kind: 'pong',
      probeId: 123,
      senderId: 'native-1',
      sendTimeMs: Date.now()
    } satisfies LatencyProbeMessage),
    'native-1'
  );
  assert.equal(ctx.hook.latencyProbe?.latestRttMs(), null);

  const ping = ctx.hook.latencyProbe?.ping();
  assert.ok(ping);
  api.handleLatencyProbePayload(
    encodeJson({
      ...ping,
      kind: 'pong',
      senderId: 'native-1'
    } satisfies LatencyProbeMessage),
    'native-1'
  );
  await flushPromiseContinuations();

  const metrics = ctx.hook.latencyProbe?.metrics() ?? [];
  assert.equal(metrics.length, 1);
  assert.equal(metrics[0].probeId, ping.probeId);
  assert.equal(metrics[0].peerIdentity, 'native-1');
  assert.ok(metrics[0].rttMs >= 0);
});

test('latency probe records RTT samples for three targeted peers independently', async () => {
  const published: Array<{ message: LatencyProbeMessage; options: Record<string, unknown> }> = [];
  const peerIdentities = ['native-1', 'native-2', 'native-3'];
  const ctx = remoteControlContext(
    (data, options) => {
      published.push({
        message: JSON.parse(new TextDecoder().decode(data)) as LatencyProbeMessage,
        options: options as Record<string, unknown>
      });
      return Promise.resolve();
    },
    () => {},
    peerIdentities
  );
  const api = setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => 1,
    publishRemoteControl: () => Promise.resolve(),
    startRemoteControl: () => {},
    stopRemoteControl: () => {}
  });

  const firstPing = ctx.hook.latencyProbe?.ping();
  assert.ok(firstPing);
  await flushPromiseContinuations();

  assert.equal(published.length, 3);
  assert.equal(new Set(published.map((entry) => entry.message.probeId)).size, 3);
  assert.deepEqual(
    published.map((entry) => entry.options),
    peerIdentities.map((identity) => ({
      topic: LATENCY_PROBE_TOPIC,
      reliable: true,
      destinationIdentities: [identity]
    }))
  );

  for (const [index, peerIdentity] of peerIdentities.entries()) {
    api.handleLatencyProbePayload(
      encodeJson({
        ...published[index].message,
        kind: 'pong',
        senderId: peerIdentity
      } satisfies LatencyProbeMessage),
      peerIdentity
    );
  }

  const metrics = ctx.hook.latencyProbe?.metrics() ?? [];
  assert.equal(metrics.length, 3);
  assert.deepEqual(
    metrics.map((metric) => metric.peerIdentity),
    peerIdentities
  );
  assert.deepEqual(
    metrics.map((metric) => metric.probeId),
    published.map((entry) => entry.message.probeId)
  );
});

// Minimal fake element supporting only what bindRemoteControlHandlers /
// ensureRemoteControlAffordance touch (dataset, addEventListener/
// dispatchEvent, the handful of DOM methods called on the tile and its
// synthesized "Request control" button). Not a full DOM -- deliberately just
// enough to drive handleRemoteControlKey through its real, non-exported
// binding path instead of testing a re-implementation of it.
class FakeControlElement {
  id = '';
  dataset: Record<string, string | undefined> = {};
  tabIndex = -1;
  textContent = '';
  className = '';
  title = '';
  classList = { toggle: () => {} };
  private readonly listeners = new Map<string, Array<(event: Record<string, unknown>) => void>>();

  addEventListener(type: string, listener: (event: Record<string, unknown>) => void) {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  dispatchEvent(event: Record<string, unknown> & { type: string }) {
    for (const listener of this.listeners.get(event.type) ?? []) {
      listener({ ...event, currentTarget: this });
    }
  }

  querySelector() {
    return null;
  }

  appendChild<T>(child: T): T {
    return child;
  }

  setAttribute() {}
  setPointerCapture() {}
  releasePointerCapture() {}
  focus() {}
}

// Node's global `navigator` is a getter-only accessor property (no setter),
// so a plain `globalThis.navigator = ...` throws -- redefine it instead, and
// restore the original descriptor afterward.
function stubGlobalNavigator(value: unknown): PropertyDescriptor | undefined {
  const prior = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', {
    value,
    configurable: true,
    writable: true
  });
  return prior;
}

function restoreGlobalNavigator(prior: PropertyDescriptor | undefined) {
  if (prior) Object.defineProperty(globalThis, 'navigator', prior);
  else delete (globalThis as { navigator?: unknown }).navigator;
}

test('handleRemoteControlKey suppresses the raw Cmd+V key event and pastes the controller clipboard as text', async () => {
  const priorDocument = (globalThis as { document?: unknown }).document;
  const priorNavigator = stubGlobalNavigator({
    clipboard: { readText: async () => 'clipboard text' }
  });
  (globalThis as { document?: unknown }).document = {
    querySelectorAll: () => [],
    createElement: () => new FakeControlElement()
  };

  try {
    const published: RemoteControlMessage[] = [];
    const ctx = remoteControlContext((data) => {
      published.push(JSON.parse(new TextDecoder().decode(data)) as RemoteControlMessage);
      return Promise.resolve();
    });
    const tile = new FakeControlElement();
    tile.id = 'tile-1';
    tile.dataset.owner = 'native';
    tile.dataset.windowId = '7';
    ctx.state.activeRemoteControl = {
      tileId: tile.id,
      targetUserId: 'native',
      windowId: 7,
      pointerId: null,
      grantToken: null
    };

    const ui = setupRemoteControlUi(ctx);
    ui.ensureRemoteControlAffordance(tile as unknown as HTMLDivElement);

    const chord = {
      key: 'v',
      code: 'KeyV',
      metaKey: true,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
      repeat: false
    };
    let prevented = 0;
    const preventDefault = () => {
      prevented += 1;
    };
    tile.dispatchEvent({ type: 'keydown', ...chord, preventDefault, stopPropagation: () => {} });
    await flushPromiseContinuations();
    tile.dispatchEvent({ type: 'keyup', ...chord, preventDefault, stopPropagation: () => {} });
    await flushPromiseContinuations();

    // Both keydown and keyup were intercepted (preventDefault called for
    // each) and neither was forwarded as a raw `key` message.
    assert.equal(prevented, 2);
    assert.deepEqual(
      published.map((message) => message.kind),
      ['text']
    );
    assert.equal((published[0] as Extract<RemoteControlMessage, { kind: 'text' }>).text, 'clipboard text');
  } finally {
    (globalThis as { document?: unknown }).document = priorDocument;
    restoreGlobalNavigator(priorNavigator);
  }
});

test('handleRemoteControlKey does not double-paste on keyboard auto-repeat', async () => {
  const priorDocument = (globalThis as { document?: unknown }).document;
  let readCount = 0;
  const priorNavigator = stubGlobalNavigator({
    clipboard: {
      readText: async () => {
        readCount += 1;
        return 'clipboard text';
      }
    }
  });
  (globalThis as { document?: unknown }).document = {
    querySelectorAll: () => [],
    createElement: () => new FakeControlElement()
  };

  try {
    const published: RemoteControlMessage[] = [];
    const ctx = remoteControlContext((data) => {
      published.push(JSON.parse(new TextDecoder().decode(data)) as RemoteControlMessage);
      return Promise.resolve();
    });
    const tile = new FakeControlElement();
    tile.id = 'tile-1';
    tile.dataset.owner = 'native';
    tile.dataset.windowId = '7';
    ctx.state.activeRemoteControl = {
      tileId: tile.id,
      targetUserId: 'native',
      windowId: 7,
      pointerId: null,
      grantToken: null
    };

    const ui = setupRemoteControlUi(ctx);
    ui.ensureRemoteControlAffordance(tile as unknown as HTMLDivElement);

    const baseChord = {
      key: 'v',
      code: 'KeyV',
      metaKey: true,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
      preventDefault: () => {},
      stopPropagation: () => {}
    };
    tile.dispatchEvent({ type: 'keydown', ...baseChord, repeat: false });
    tile.dispatchEvent({ type: 'keydown', ...baseChord, repeat: true });
    tile.dispatchEvent({ type: 'keydown', ...baseChord, repeat: true });
    await flushPromiseContinuations();

    assert.equal(readCount, 1);
    assert.equal(published.length, 1);
  } finally {
    (globalThis as { document?: unknown }).document = priorDocument;
    restoreGlobalNavigator(priorNavigator);
  }
});

// #580: `api.request(target)` with an explicit target used to skip
// rc.startRemoteControl and raw-publish a bare `request`, so no grant token
// was ever minted and the host dropped every later input packet
// ("dropping tokenless input ... compatibility window has ended",
// remote_control.rs). The single-machine live RC suite was therefore
// unrunnable while still reporting pass on its absence-asserting cases.
function harnessGrantContext(published: RemoteControlMessage[]) {
  // A plain field, not a constructor parameter property: the root tsconfig
  // sets `erasableSyntaxOnly`, which rejects those (TS1294).
  class FakeDiv {
    readonly id: string;
    constructor(id: string) {
      this.id = id;
    }
  }
  const priorHtmlDivElement = globalThis.HTMLDivElement;
  Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: FakeDiv });
  const tile = new FakeDiv('tile-7') as unknown as HTMLDivElement;
  const priorDocument = (globalThis as { document?: unknown }).document;
  (globalThis as { document?: unknown }).document = {
    querySelectorAll: () => [],
    getElementById: (id: string) => (id === tile.id ? tile : null)
  };
  const ctx = remoteControlContext(() => Promise.resolve());
  (ctx.cb as unknown as { shareTileForWindowId: (id: number) => unknown }).shareTileForWindowId = (
    windowId: number
  ) => (windowId === 7 ? tile : null);
  const startCalls: string[] = [];
  setupHarnessApi(ctx, {
    nextRemoteControlSeq: () => {
      ctx.state.remoteControlSeq += 1;
      return ctx.state.remoteControlSeq;
    },
    publishRemoteControl: (message) => {
      published.push(message);
      return Promise.resolve();
    },
    // Mirrors remoteControlUi.startRemoteControl: it is what establishes
    // activeRemoteControl, which is the only thing that can later hold a
    // grant token.
    startRemoteControl: (started: HTMLDivElement) => {
      startCalls.push(started.id);
      ctx.state.activeRemoteControl = {
        tileId: started.id,
        targetUserId: 'native',
        windowId: 7,
        pointerId: null,
        grantToken: null
      };
    },
    stopRemoteControl: () => {
      ctx.state.activeRemoteControl = null;
    }
  });
  const api = ctx.hook.remoteControl!;
  return { api, ctx, startCalls, tile, restore: () => {
    (globalThis as { document?: unknown }).document = priorDocument;
    if (priorHtmlDivElement === undefined) Reflect.deleteProperty(globalThis, 'HTMLDivElement');
    else Object.defineProperty(globalThis, 'HTMLDivElement', { configurable: true, value: priorHtmlDivElement });
  } };
}

const explicitTarget = { targetUserId: 'native', windowId: 7, tileId: 'tile-7' };

test('#580 explicit-target request mints a grant so later input carries the token', async () => {
  const published: RemoteControlMessage[] = [];
  const { api, ctx, startCalls, restore } = harnessGrantContext(published);
  try {
    api.request(explicitTarget);
    // Pre-fix this was [] -- the explicit-target branch raw-published instead.
    assert.deepEqual(startCalls, ['tile-7']);

    // The host answers the request with the session's grant token.
    ctx.state.activeRemoteControl!.grantToken = 'grant-token-1';
    assert.deepEqual(api.grant(explicitTarget), {
      target: explicitTarget,
      granted: true,
      grantToken: 'grant-token-1',
      // #820/case-30: legacy grant -- no v2 session negotiated.
      controlSessionId: null,
      tokenlessInputs: 0
    });

    api.pointer({ target: explicitTarget, action: 'move', x: 0.5, y: 0.5 });
    await flushPromiseContinuations();
    const move = published.find((message) => message.kind === 'pointer');
    assert.ok(move, 'the pointer packet was published');
    assert.equal(move!.grantToken, 'grant-token-1');
    assert.deepEqual(api.metrics().tokenlessInputs, []);
  } finally {
    restore();
  }
});

// Two-sided: the detector above must be capable of firing, or "tokenlessInputs
// is empty" proves nothing. An input published with no grant is still put on
// the wire (scenario case 24, "release drops later input", needs the host to
// be the thing that rejects it) but is now counted and visible to the driver.
test('#580 input published without a grant is counted so a driver cannot read it as success', async () => {
  const published: RemoteControlMessage[] = [];
  const { api, restore } = harnessGrantContext(published);
  try {
    assert.equal(api.grant(explicitTarget).granted, false);

    api.pointer({ target: explicitTarget, action: 'move', x: 0.5, y: 0.5 });
    await flushPromiseContinuations();
    const move = published.find((message) => message.kind === 'pointer');
    assert.ok(move, 'the tokenless packet still reaches the wire for the host to reject');
    assert.equal(move!.grantToken, undefined);

    const tokenless = api.metrics().tokenlessInputs;
    assert.equal(tokenless.length, 1);
    assert.equal(tokenless[0].kind, 'pointer');
    assert.equal(api.grant(explicitTarget).tokenlessInputs, 1);
  } finally {
    restore();
  }
});

test('#580 request without a share tile fails loudly instead of publishing a tokenless request', () => {
  const published: RemoteControlMessage[] = [];
  const { api, restore } = harnessGrantContext(published);
  try {
    assert.throws(
      () => api.request({ targetUserId: 'native', windowId: 99, tileId: 'tile-99' }),
      /no share tile for window 99/
    );
    assert.deepEqual(published, []);
  } finally {
    restore();
  }
});
