import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  HOST_API_VERSION,
  MANIFEST_LIMITS,
  compareVersions,
  hostCompatibility,
  isNetHostPermission,
  isPluginId,
  isReleaseVersion,
  releaseCore,
  validateManifest,
} from '@petal/shared/plugin-host/manifest';

function base(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    manifestVersion: 1,
    id: 'petal.reactions',
    version: '1.0.0',
    name: 'Reactions',
    description: 'Emoji reactions for everyone in the meeting.',
    apiVersion: 1,
    minHostVersion: '0.10.0',
    scope: 'meeting',
    entry: 'plugin.js',
    permissions: ['meeting:read', 'data:publish', 'ui:toolbar-button', 'ui:popover', 'ui:overlay'],
    contributes: {
      toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile', opens: 'popover:picker' }],
      surfaces: { overlay: { id: 'fx' }, popover: { id: 'picker', width: 280, height: 120 } },
    },
    native: null,
    ...over,
  };
}

function errorsOf(input: unknown): string[] {
  const r = validateManifest(input);
  return r.ok ? [] : r.errors;
}

test('a well-formed meeting manifest validates and is normalised', () => {
  const r = validateManifest(base({ name: '  Reactions ' }));
  assert.ok(r.ok);
  if (!r.ok) return;
  assert.equal(r.manifest.name, 'Reactions');
  assert.deepEqual(r.warnings, []);
  assert.equal(r.manifest.native, null);
});

test('id, version, name, description and entry rules', () => {
  assert.ok(isPluginId('acme.my-plugin'));
  assert.ok(!isPluginId('reactions'));
  assert.ok(!isPluginId('Acme.Reactions'));
  assert.ok(!isPluginId('a'.repeat(70) + '.x'));
  assert.ok(isReleaseVersion('1.0.0'));
  assert.ok(!isReleaseVersion('1.0.0-beta.1'));
  assert.match(errorsOf(base({ id: 'reactions' })).join('\n'), /publisher-prefixed/);
  assert.match(errorsOf(base({ version: '1.0' })).join('\n'), /strict major.minor.patch/);
  assert.match(errorsOf(base({ name: 'x'.repeat(MANIFEST_LIMITS.nameMaxLength + 1) })).join('\n'), /name must be/);
  assert.match(errorsOf(base({ description: 'x'.repeat(200) })).join('\n'), /description/);
  assert.match(errorsOf(base({ entry: '../plugin.js' })).join('\n'), /entry/);
  assert.match(errorsOf(base({ entry: 'plugin.mjs' })).join('\n'), /entry/);
  assert.match(errorsOf(base({ manifestVersion: 2 })).join('\n'), /manifestVersion/);
  assert.match(errorsOf('nope').join('\n'), /JSON object/);
});

test('permission vocabulary: unknown, reserved, wildcard, duplicates', () => {
  assert.match(errorsOf(base({ permissions: ['meeting:read', 'fs:read'] })).join('\n'), /unknown permission "fs:read"/);
  assert.match(errorsOf(base({ permissions: ['frames:read'] })).join('\n'), /not supported by this host/);
  assert.match(errorsOf(base({ permissions: ['net:fetch:*'] })).join('\n'), /net:fetch:\* is not allowed/);
  assert.match(errorsOf(base({ permissions: ['storage', 'storage'] })).join('\n'), /duplicate permission/);
  assert.ok(isNetHostPermission('net:fetch:hooks.slack.com'));
  assert.ok(isNetHostPermission('net:fetch:*.example.com'));
  assert.ok(isNetHostPermission('net:fetch:localhost:8787'));
  assert.ok(!isNetHostPermission('net:fetch:user-urls'));
  assert.ok(!isNetHostPermission('net:fetch:https://x.com'));
});

test('local scope cannot talk to the meeting', () => {
  const errors = errorsOf(base({ scope: 'local', permissions: ['meeting:read', 'data:publish', 'state:write'], contributes: undefined }));
  assert.match(errors.join('\n'), /"data:publish" requires scope "meeting"/);
  assert.match(errors.join('\n'), /"state:write" requires scope "meeting"/);
  const ok = validateManifest(base({ scope: 'local', permissions: ['meeting:read', 'storage'], contributes: undefined }));
  assert.ok(ok.ok);
});

test('contributions require their ui permission and reference declared surfaces', () => {
  const missing = errorsOf(base({ permissions: ['meeting:read', 'data:publish'] }));
  assert.match(missing.join('\n'), /toolbarButtons requires permission "ui:toolbar-button"/);
  assert.match(missing.join('\n'), /surfaces.overlay requires permission "ui:overlay"/);
  assert.match(missing.join('\n'), /surfaces.popover requires permission "ui:popover"/);

  const dangling = errorsOf(base({ contributes: { toolbarButtons: [{ id: 'react', label: 'React', icon: 'smile', opens: 'popover:nope' }] } }));
  assert.match(dangling.join('\n'), /opens must name a declared surface/);

  const longLabel = errorsOf(base({ contributes: { toolbarButtons: [{ id: 'react', label: 'A label far too long', icon: 'smile' }] } }));
  assert.match(longLabel.join('\n'), /UI text must never truncate/);

  const dup = errorsOf(
    base({
      contributes: {
        toolbarButtons: [
          { id: 'react', label: 'React', icon: 'smile' },
          { id: 'react', label: 'Again', icon: 'smile' },
        ],
      },
    }),
  );
  assert.match(dup.join('\n'), /duplicate id "react"/);

  const unknownKind = errorsOf(base({ contributes: { surfaces: { sidebar: { id: 'x' } } } }));
  assert.match(unknownKind.join('\n'), /unknown surface kind/);
});

test('settings fields: url + netAllow needs net:fetch:user-urls; boolean cannot netAllow', () => {
  const ok = validateManifest(
    base({
      scope: 'local',
      permissions: ['meeting:read', 'storage', 'net:fetch:user-urls', 'ui:settings'],
      contributes: {
        surfaces: { settings: { id: 'config' } },
        settings: [{ key: 'webhook-url', type: 'url', label: 'Webhook URL', netAllow: true }],
      },
    }),
  );
  assert.ok(ok.ok, JSON.stringify(ok));
  const bad = errorsOf(
    base({
      scope: 'local',
      permissions: ['meeting:read', 'ui:settings'],
      contributes: { settings: [{ key: 'flag', type: 'boolean', label: 'Flag', netAllow: true }] },
    }),
  );
  assert.match(bad.join('\n'), /netAllow is only valid on "url" fields/);
  assert.match(bad.join('\n'), /netAllow requires permission "net:fetch:user-urls"/);
});

test('native slot is accepted with a warning, garbage is rejected', () => {
  const r = validateManifest(base({ native: { wasm: 'native/plugin.wasm', abi: 'petal-native-v0' } }));
  assert.ok(r.ok);
  if (r.ok) assert.match(r.warnings.join('\n'), /native tier is not supported/);
  assert.match(errorsOf(base({ native: { wasm: 1 } })).join('\n'), /native must be null or/);
});

test('host compatibility: api major and minimum host version', () => {
  assert.equal(compareVersions('1.10.0', '1.9.0'), 1);
  assert.deepEqual(hostCompatibility({ apiVersion: 1, minHostVersion: '0.10.0' }, '0.10.0'), { ok: true });
  assert.deepEqual(hostCompatibility({ apiVersion: 1, minHostVersion: '0.10.0' }, '0.11.2-dev+abc'), { ok: true });
  const old = hostCompatibility({ apiVersion: 1, minHostVersion: '0.10.0' }, '0.9.7');
  assert.equal(old.ok, false);
  const future = hostCompatibility({ apiVersion: HOST_API_VERSION + 1, minHostVersion: '0.1.0' }, '9.9.9');
  assert.equal(future.ok, false);
  assert.equal(releaseCore('0.9.7-dev+abc'), '0.9.7');
  assert.equal(releaseCore('dev'), null);
});
