import { test } from 'node:test';
import assert from 'node:assert/strict';

import { BRIDGE_METHODS, HOST_EVENTS } from '@petal/shared/plugin-host/protocol';
import {
  EVENT_PERMISSIONS,
  METHOD_PERMISSIONS,
  hostMatches,
  netFetchAllowed,
  newPermissions,
} from '@petal/shared/plugin-host/permissions';

test('every bridge method and host event has an explicit permission entry', () => {
  for (const m of BRIDGE_METHODS) assert.ok(m in METHOD_PERMISSIONS, m);
  for (const e of HOST_EVENTS) assert.ok(e in EVENT_PERMISSIONS, e);
});

test('hostMatches: exact and wildcard-subdomain only', () => {
  assert.ok(hostMatches('hooks.slack.com', 'hooks.slack.com'));
  assert.ok(hostMatches('hooks.slack.com', 'HOOKS.slack.com'));
  assert.ok(!hostMatches('slack.com', 'hooks.slack.com'));
  assert.ok(hostMatches('*.example.com', 'a.example.com'));
  assert.ok(hostMatches('*.example.com', 'a.b.example.com'));
  assert.ok(!hostMatches('*.example.com', 'example.com'));
  assert.ok(!hostMatches('*.example.com', 'notexample.com'));
});

test('netFetchAllowed: https only, allowlisted hosts, user origins', () => {
  const granted = ['net:fetch:hooks.slack.com', 'net:fetch:*.example.com', 'net:fetch:user-urls'] as const;
  const ctx = { granted: [...granted], userOrigins: ['https://my.webhook.test'] };
  assert.equal(netFetchAllowed(ctx, 'https://hooks.slack.com/services/x').ok, true);
  assert.equal(netFetchAllowed(ctx, 'https://api.example.com/v1').ok, true);
  assert.equal(netFetchAllowed(ctx, 'https://my.webhook.test/hook').ok, true);
  assert.equal(netFetchAllowed(ctx, 'https://my.webhook.test:8443/hook').ok, false, 'different origin (port)');
  assert.equal(netFetchAllowed(ctx, 'http://hooks.slack.com/x').ok, false, 'http refused');
  assert.equal(netFetchAllowed(ctx, 'https://evil.com/').ok, false);
  assert.equal(netFetchAllowed(ctx, 'https://user:pw@hooks.slack.com/').ok, false, 'credentials refused');
  assert.equal(netFetchAllowed(ctx, 'not a url').ok, false);
  assert.equal(netFetchAllowed({ granted: ['net:fetch:localhost:8787'] }, 'http://localhost:8787/x').ok, true);
  assert.equal(netFetchAllowed({ granted: ['net:fetch:localhost:8787'] }, 'http://localhost:9999/x').ok, false);
  assert.equal(netFetchAllowed({ granted: ['net:fetch:user-urls'] }, 'https://my.webhook.test/').ok, false, 'no user origins recorded');
});

test('newPermissions reports only additions', () => {
  assert.deepEqual(newPermissions(['storage'], ['storage', 'ui:toast']), ['ui:toast']);
  assert.deepEqual(newPermissions(['storage', 'ui:toast'], ['storage']), []);
});
