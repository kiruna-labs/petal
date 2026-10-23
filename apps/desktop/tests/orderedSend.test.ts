import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { createOrderedSender } from '../src/lib/orderedSend.ts';

function delay(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test('sends reach the transport in call order even when an earlier send is slower', async () => {
  const delivered: string[] = [];
  // The Down's transport call is slower than the Up's: unordered invokes
  // would deliver the Up first, which is exactly the reversal the host saw.
  const latency: Record<string, number> = { down: 15, up: 0 };
  const send = createOrderedSender(async (action: string) => {
    await delay(latency[action] ?? 0);
    delivered.push(action);
  });
  void send('down');
  await send('up');
  assert.deepEqual(delivered, ['down', 'up']);
});

test('a later send starts only after the previous one settled', async () => {
  let inFlight = 0;
  let maxInFlight = 0;
  const send = createOrderedSender(async () => {
    inFlight += 1;
    maxInFlight = Math.max(maxInFlight, inFlight);
    await delay(2);
    inFlight -= 1;
  });
  await Promise.all([send(1), send(2), send(3), send(4)]);
  assert.equal(maxInFlight, 1);
});

test('a failed send neither rejects its caller nor stalls the queue', async () => {
  const delivered: number[] = [];
  const send = createOrderedSender(async (n: number) => {
    if (n === 2) throw new Error('publish failed');
    delivered.push(n);
  });
  await assert.doesNotReject(send(1));
  await assert.doesNotReject(send(2));
  await send(3);
  assert.deepEqual(delivered, [1, 3]);
});

test('the control overlay routes remote_control_send through the ordered sender', () => {
  const page = readFileSync(
    new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
    'utf8'
  );
  const sendBody = page.slice(page.indexOf('function send(draft'), page.indexOf('function isClipboardModifier'));
  assert.match(sendBody, /sendOrdered\(draft\)/);
  assert.doesNotMatch(sendBody, /invoke\(COMMANDS\.remoteControlSend/);
  assert.match(page, /createOrderedSender\(\(draft[^)]*\) =>\s*invoke\(COMMANDS\.remoteControlSend/);
});
