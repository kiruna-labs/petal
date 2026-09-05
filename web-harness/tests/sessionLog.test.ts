import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  createSessionLogFilename,
  SessionLogCollector,
} from '../src/ui/sessionLogCollector.ts';

test('session log collector rotates as a ring buffer', () => {
  const collector = new SessionLogCollector(3);

  for (let i = 1; i <= 5; i += 1) {
    collector.record({
      ts: `2026-07-06T00:00:0${i}.000Z`,
      identity: `web-${i}`,
      room: 'abc-defg-hjk',
      kind: 'info',
      message: `event ${i}`,
    });
  }

  assert.deepEqual(
    collector.getEntries().map((entry) => entry.message),
    ['event 3', 'event 4', 'event 5']
  );
});

test('session log export includes timestamp, identity, room, kind, and one-line message', async () => {
  const collector = new SessionLogCollector();

  collector.record({
    ts: '2026-07-06T21:22:23.456Z',
    identity: 'web-bob',
    room: 'petal-room-123',
    kind: 'warn',
    message: 'first line\nsecond line',
  });

  const expected = '2026-07-06T21:22:23.456Z web-bob petal-room-123 [warn] first line\\nsecond line';
  assert.equal(collector.exportText(), expected);
  assert.equal(await collector.exportBlob().text(), expected);
});

test('session log filename includes sanitized identity, room, and timestamp', () => {
  const filename = createSessionLogFilename(
    {
      identity: 'web:bob/example',
      room: 'Design Review/Room',
    },
    new Date('2026-07-06T21:22:23.456Z')
  );

  assert.equal(filename, 'petal-session-web-bob-example-Design-Review-Room-2026-07-06T21-22-23-456Z.log');
});
