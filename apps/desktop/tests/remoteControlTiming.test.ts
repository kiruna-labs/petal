import assert from 'node:assert/strict';
import test from 'node:test';

import {
  remoteControlInputRttMs,
  stampSentAtMs,
  wireMs
} from '@petal/shared/logic/remoteControlTiming';

test('#371: stampSentAtMs adds an integer stamp and keeps an existing one', () => {
  assert.deepEqual(stampSentAtMs({ kind: 'key', seq: 3 }, 1_700_000_000_123.7), {
    kind: 'key',
    seq: 3,
    sentAtMs: 1_700_000_000_123
  });
  const produced = { kind: 'text', sentAtMs: 42 };
  assert.equal(stampSentAtMs(produced, 99), produced, 'production stamp wins');
  assert.equal(stampSentAtMs({ sentAtMs: -1 }, 7).sentAtMs, 7, 'an invalid stamp is replaced');
});

test('#371: input rtt subtracts the host hold and needs it', () => {
  assert.equal(remoteControlInputRttMs(1_000, 1_090, 12), 78);
  assert.equal(remoteControlInputRttMs(1_000, 1_090, undefined), undefined, 'old host');
  assert.equal(remoteControlInputRttMs(undefined, 1_090, 12), undefined, 'unstamped send');
  assert.equal(remoteControlInputRttMs(1_000, 1_005, 10), undefined, 'negative is dropped');
  assert.equal(wireMs(1.5), undefined);
  assert.equal(wireMs('5'), undefined);
  assert.equal(wireMs(0), 0);
});
