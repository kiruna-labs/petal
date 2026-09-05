import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(
  new URL('../src/lib/components/ControlButton.svelte', import.meta.url),
  'utf8'
);

const remoteControlBranch = source.match(
  /\{:else if icon === 'remotecontrol'\}([\s\S]*?)\{:else if icon === 'invite'\}/
)?.[1];

assert.ok(remoteControlBranch, 'remotecontrol icon branch should exist');

test('remote-control icon uses canonical telepointer path in both layers', () => {
  const telepointerPaths = remoteControlBranch.match(/d="M5 3l5 16 2\.5-6\.5L19 10z"/g) ?? [];

  assert.equal(telepointerPaths.length, 2);
  assert.doesNotMatch(remoteControlBranch, /M5 3v16l4\.5-4\.5/);
});

test('remote-control off slash runs bottom-left to top-right only for that icon', () => {
  assert.match(remoteControlBranch, /d="M3 21L21 3"/);
  assert.doesNotMatch(remoteControlBranch, /d="M3 3l18 18"/);

  assert.match(source, /class="mic-slash"[^>]*d="M3 3l18 18"/);
  assert.match(source, /<path d="M2 2l20 20"><\/path>/);
});
