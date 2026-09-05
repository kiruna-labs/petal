import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  applyLocalEchoKey,
  clampLocalEchoAnchor,
  LOCAL_ECHO_RIPPLE_FADE_MS,
  LOCAL_ECHO_TEXT_TIMEOUT_MS,
  nextLocalEchoRippleId,
} from '@petal/shared/logic/localEcho';

// -------------------------------------------------------------------------
// Pure logic: mirrors apps/desktop/tests/localEcho.test.ts's coverage of the
// shared apps/desktop/src/lib/data/localEcho.ts module exactly, since the
// two implementations are meant to behave identically (#378 parity).
// -------------------------------------------------------------------------

test('constants match the issue-specified timings (#378)', () => {
  assert.equal(LOCAL_ECHO_RIPPLE_FADE_MS, 150);
  assert.equal(LOCAL_ECHO_TEXT_TIMEOUT_MS, 2000);
});

test('nextLocalEchoRippleId increments and wraps at MAX_SAFE_INTEGER', () => {
  assert.equal(nextLocalEchoRippleId(0), 1);
  assert.equal(nextLocalEchoRippleId(41), 42);
  assert.equal(nextLocalEchoRippleId(Number.MAX_SAFE_INTEGER), 1);
});

test('applyLocalEchoKey appends printable single-codepoint characters', () => {
  const base = { ctrlKey: false, metaKey: false, altKey: false };
  assert.equal(applyLocalEchoKey('', { ...base, key: 'a' }), 'a');
  assert.equal(applyLocalEchoKey('ab', { ...base, key: 'c' }), 'abc');
  assert.equal(applyLocalEchoKey('', { ...base, key: ' ' }), ' ');
  assert.equal(applyLocalEchoKey('', { ...base, key: '😀' }), '😀');
});

test('applyLocalEchoKey ignores shortcuts (ctrl/meta/alt combos)', () => {
  assert.equal(applyLocalEchoKey('abc', { key: 'a', ctrlKey: true, metaKey: false, altKey: false }), null);
  assert.equal(applyLocalEchoKey('abc', { key: 'c', ctrlKey: false, metaKey: true, altKey: false }), null);
  assert.equal(applyLocalEchoKey('abc', { key: 'v', ctrlKey: false, metaKey: false, altKey: true }), null);
});

test('applyLocalEchoKey pops on Backspace and clears on Enter', () => {
  const base = { ctrlKey: false, metaKey: false, altKey: false };
  assert.equal(applyLocalEchoKey('abc', { ...base, key: 'Backspace' }), 'ab');
  assert.equal(applyLocalEchoKey('', { ...base, key: 'Backspace' }), '');
  assert.equal(applyLocalEchoKey('hello', { ...base, key: 'Enter' }), '');
});

test('applyLocalEchoKey leaves non-text keys (arrows, function keys) untouched', () => {
  const base = { ctrlKey: false, metaKey: false, altKey: false };
  assert.equal(applyLocalEchoKey('abc', { ...base, key: 'ArrowLeft' }), null);
  assert.equal(applyLocalEchoKey('abc', { ...base, key: 'F5' }), null);
  assert.equal(applyLocalEchoKey('abc', { ...base, key: 'Shift' }), null);
});

test('clampLocalEchoAnchor keeps the pending-text anchor inside the tile bounds', () => {
  const bounds = { width: 400, height: 300 };
  assert.deepEqual(clampLocalEchoAnchor({ x: 200, y: 150 }, bounds), { x: 200, y: 150 });
  assert.deepEqual(clampLocalEchoAnchor({ x: -50, y: -50 }, bounds), { x: 12, y: 12 });
  assert.deepEqual(clampLocalEchoAnchor({ x: 5000, y: 5000 }, bounds), { x: 388, y: 288 });
});

// -------------------------------------------------------------------------
// Setting: opt-in, default OFF (mirrors desktop's session.localEchoEnabled)
// -------------------------------------------------------------------------

const constantsSource = readFileSync(new URL('../src/constants.ts', import.meta.url), 'utf8');
const contextSource = readFileSync(new URL('../src/context.ts', import.meta.url), 'utf8');
const mainSource = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const indexHtmlSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const remoteControlUiSource = readFileSync(new URL('../src/remoteControlUi.ts', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');

test('the local echo checkbox exists in the dev panel, unchecked by default', () => {
  assert.match(indexHtmlSource, /<input type="checkbox" id="local-echo-checkbox" \/>/);
  assert.doesNotMatch(indexHtmlSource, /id="local-echo-checkbox"[^>]*checked/);
});

test('HarnessState carries localEchoEnabled and it is seeded from persisted storage, default off', () => {
  assert.match(constantsSource, /HARNESS_LOCAL_ECHO_STORAGE_KEY = 'petal-harness-local-echo-enabled';/);
  assert.match(contextSource, /localEchoEnabled: boolean;/);
  assert.match(
    mainSource,
    /localEchoEnabled: localStorage\.getItem\(HARNESS_LOCAL_ECHO_STORAGE_KEY\) === '1',/
  );
});

test('the checkbox change handler persists the choice without any wire/room dependency', () => {
  const fnMatch = controlsSource.match(
    /localEchoCheckbox\.addEventListener\('change', \(\) => \{[\s\S]*?\n {4}\}\);/
  );
  assert.ok(fnMatch, 'a change listener for localEchoCheckbox must be registered');
  const fnBody = fnMatch[0];
  assert.match(fnBody, /state\.localEchoEnabled = localEchoCheckbox\.checked;/);
  assert.match(fnBody, /HARNESS_LOCAL_ECHO_STORAGE_KEY/);
  // This is a local-rendering-only toggle -- must not gate on state.room or
  // touch the LiveKit room/publish path the way the real mic toggle does.
  assert.doesNotMatch(fnBody, /state\.room/);
});

// -------------------------------------------------------------------------
// remoteControlUi.ts wiring: gated behind the setting, zero wire changes
// -------------------------------------------------------------------------

test('local echo hooks pointerdown, wheel, and keydown SEND paths behind state.localEchoEnabled', () => {
  assert.match(remoteControlUiSource, /if \(state\.localEchoEnabled\) \{\s*\n\s*echoLastClickPoint = spawnEchoRipple/);
  assert.match(remoteControlUiSource, /if \(state\.localEchoEnabled && !wheelFrame\) \{/);
  assert.match(remoteControlUiSource, /if \(state\.localEchoEnabled && action === 'down'\) \{\s*\n\s*handleEchoKeydown/);
});

test('local echo state is cleared when control stops, whether locally or via a host status packet', () => {
  const stopBlock = remoteControlUiSource.match(/function stopRemoteControl\(reason\?: string\) \{[\s\S]*?\n {2}\}/);
  assert.ok(stopBlock, 'stopRemoteControl must exist');
  assert.match(
    stopBlock[0],
    /if \(state\.localEchoEnabled\) \{\s*\n\s*clearLocalEcho\(document\.getElementById\(stopped\.tileId\) as HTMLDivElement \| null\);/
  );

  const payloadBlock = remoteControlUiSource.match(
    /\} else if \([\s\S]*?message\.status === 'stopped'[\s\S]*?\n\s*\) \{[\s\S]*?\n\s{6}\}/
  );
  assert.ok(payloadBlock, 'the host "stopped" status branch must exist');
  assert.match(
    payloadBlock[0],
    /if \(state\.localEchoEnabled\) \{\s*\n\s*clearLocalEcho\(document\.getElementById\(active\.tileId\) as HTMLDivElement \| null\);/
  );
});

test('the pending-text strip is always labeled as unconfirmed, never as real content', () => {
  assert.match(remoteControlUiSource, /sent, unconfirmed/);
  assert.match(remoteControlUiSource, /layer\.setAttribute\('aria-hidden', 'true'\);/);
  assert.match(styleSource, /\.local-echo-layer \{/);
  assert.match(styleSource, /\.local-echo-text-badge \{/);
});
