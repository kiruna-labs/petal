import assert from 'node:assert/strict';
import test from 'node:test';

import { joinRoomCommandPayload } from '../src/lib/data/rooms.ts';
import { migrateRemoteControlPolicy } from '../src/lib/remoteControlPolicy.ts';

test('join room payload carries the remote-control policy (default ask) plus the legacy boolean', () => {
  assert.deepEqual(joinRoomCommandPayload('eng-sync', 'alice', 'Alice'), {
    roomName: 'eng-sync',
    identity: 'alice',
    displayName: 'Alice',
    remoteControlAllowed: true,
    remoteControlPolicy: 'ask',
    identityPaletteIndex: null
  });

  assert.deepEqual(joinRoomCommandPayload('eng-sync', 'alice', 'Alice', 'off'), {
    roomName: 'eng-sync',
    identity: 'alice',
    displayName: 'Alice',
    remoteControlAllowed: false,
    remoteControlPolicy: 'off',
    identityPaletteIndex: null
  });

  assert.deepEqual(joinRoomCommandPayload('eng-sync', 'alice', 'Alice', 'auto'), {
    roomName: 'eng-sync',
    identity: 'alice',
    displayName: 'Alice',
    remoteControlAllowed: true,
    remoteControlPolicy: 'auto',
    identityPaletteIndex: null
  });
});

test('persisted boolean migrates to a policy: true -> ask (never auto), false -> off, fresh -> ask', () => {
  assert.equal(migrateRemoteControlPolicy({}), 'ask');
  assert.equal(migrateRemoteControlPolicy({ allowRemoteControlByDefault: true }), 'ask');
  assert.equal(migrateRemoteControlPolicy({ allowRemoteControlByDefault: false }), 'off');
  assert.equal(migrateRemoteControlPolicy({ remoteControlPolicy: 'auto' }), 'auto');
  assert.equal(migrateRemoteControlPolicy({ remoteControlPolicy: 'off', allowRemoteControlByDefault: true }), 'off');
  assert.equal(migrateRemoteControlPolicy({ remoteControlPolicy: 'garbage' }), 'ask');
});
