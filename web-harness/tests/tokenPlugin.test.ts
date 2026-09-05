import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  parseQuery,
  requestAsksForHidden,
  safeGrantForRoom,
  tokenEndpointExposureError,
  UNSAFE_TOKEN_ENDPOINT_ENV,
} from '../server/tokenPlugin.ts';

test('token endpoint rejects hidden participant requests', () => {
  assert.equal(parseQuery('/api/token?room=r&identity=i&hidden=true').hidden, true);
  assert.equal(requestAsksForHidden({ hidden: true }), true);
  assert.equal(requestAsksForHidden({ hidden: 'true' }), true);
  assert.throws(
    () => safeGrantForRoom('petal-room-test', { hidden: true }),
    /hidden LiveKit participants are not allowed/,
  );
});

test('token endpoint clamps grants to the fixed safe test profile', () => {
  assert.deepEqual(
    safeGrantForRoom('petal-room-test', {
      canPublish: false,
      canSubscribe: false,
      canPublishData: false,
      hidden: false,
    }),
    {
      roomJoin: true,
      room: 'petal-room-test',
      canPublish: true,
      canSubscribe: true,
      canPublishData: true,
      canUpdateOwnMetadata: true,
      hidden: false,
    },
  );
});

test('token endpoint blocks real secrets on non-loopback dev servers unless explicitly opted in', () => {
  const blocked = tokenEndpointExposureError({
    livekitUrl: 'wss://petal-livekit.example',
    apiKey: 'real-key',
    apiSecret: 'real-secret',
    serverHost: '0.0.0.0',
    requestHost: '192.168.1.10:5173',
  });

  assert.match(blocked ?? '', /Refusing to mint LiveKit tokens/);
  assert.match(blocked ?? '', new RegExp(UNSAFE_TOKEN_ENDPOINT_ENV));

  assert.equal(
    tokenEndpointExposureError({
      livekitUrl: 'wss://petal-livekit.example',
      apiKey: 'real-key',
      apiSecret: 'real-secret',
      serverHost: 'localhost',
      requestHost: '127.0.0.1:5173',
    }),
    null,
  );

  assert.equal(
    tokenEndpointExposureError({
      livekitUrl: 'wss://petal-livekit.example',
      apiKey: 'real-key',
      apiSecret: 'real-secret',
      serverHost: '0.0.0.0',
      requestHost: '192.168.1.10:5173',
      allowUnsafeNonLoopback: true,
    }),
    null,
  );
});

test('token endpoint allows default local LiveKit dev credentials on non-loopback test servers', () => {
  assert.equal(
    tokenEndpointExposureError({
      livekitUrl: 'ws://127.0.0.1:7880',
      apiKey: 'devkey',
      apiSecret: 'secret',
      serverHost: '0.0.0.0',
      requestHost: '192.168.1.10:5173',
    }),
    null,
  );
});
