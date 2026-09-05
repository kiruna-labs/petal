import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import {
  applyLocalEchoKey,
  clampLocalEchoAnchor,
  LOCAL_ECHO_RIPPLE_FADE_MS,
  LOCAL_ECHO_TEXT_TIMEOUT_MS,
  nextLocalEchoRippleId
} from '../src/lib/data/localEcho.ts';

// `session.svelte.ts`/`+page.svelte` use Svelte 5 runes ($state) at module
// scope, so they can't be imported and executed directly under plain
// `node --test` -- same constraint sentryEnabledToggle.test.ts already works
// around by asserting against the raw source text instead.
const sessionStoreSource = readFileSync(
  new URL('../src/lib/stores/session.svelte.ts', import.meta.url),
  'utf8'
);
const settingsComponentSource = readFileSync(
  new URL('../src/lib/components/Settings.svelte', import.meta.url),
  'utf8'
);
const settingsPageSource = readFileSync(
  new URL('../src/routes/settings/+page.svelte', import.meta.url),
  'utf8'
);
const controlRouteSource = readFileSync(
  new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
  'utf8'
);

// -------------------------------------------------------------------------
// Setting: opt-in, default OFF
// -------------------------------------------------------------------------

test('localEchoEnabled defaults to false and is part of the persisted session shape', () => {
  assert.match(sessionStoreSource, /localEchoEnabled:\s*boolean;/);
  assert.match(sessionStoreSource, /localEchoEnabled:\s*false/);
  // Guard against a future edit accidentally flipping the shipped default --
  // #378's explicit user decision was opt-in, default OFF.
  assert.doesNotMatch(sessionStoreSource, /localEchoEnabled:\s*true/);
});

test('updateLocalEchoEnabled persists the new value without inventing a wire/native call', () => {
  const fnMatch = sessionStoreSource.match(
    /export function updateLocalEchoEnabled\(enabled: boolean\) \{[\s\S]*?\n\}/
  );
  assert.ok(fnMatch, 'updateLocalEchoEnabled must be exported from session.svelte.ts');
  const fnBody = fnMatch[0];
  assert.match(fnBody, /session\.localEchoEnabled = enabled;/);
  assert.match(fnBody, /persist\(session\);/);
  // This is a local-rendering-only toggle (no Rust/native counterpart, no
  // wire message) -- unlike updateRemoteControlDefault/updateSentryEnabled,
  // it must NOT invoke() into the Tauri bridge.
  assert.doesNotMatch(fnBody, /invoke\(/);
});

test('Settings.svelte exposes a local echo toggle wired to the same prop pattern as other settings', () => {
  assert.match(settingsComponentSource, /localEchoEnabled\?:\s*boolean;/);
  assert.match(settingsComponentSource, /onLocalEchoEnabledChange\?:\s*\(enabled: boolean\) => void;/);
  assert.match(settingsComponentSource, /localEchoEnabled = false,/);
  assert.match(settingsComponentSource, /checked=\{localEchoEnabled\}/);
  assert.match(
    settingsComponentSource,
    /onchange=\{\(e\) => onLocalEchoEnabledChange\?\.\(e\.currentTarget\.checked\)\}/
  );
  assert.match(settingsComponentSource, /Local echo \(experimental\)/);
});

test('the real Settings route binds the local echo toggle to the session store', () => {
  assert.match(settingsPageSource, /updateLocalEchoEnabled/);
  assert.match(settingsPageSource, /localEchoEnabled=\{session\.localEchoEnabled\}/);
  assert.match(settingsPageSource, /onLocalEchoEnabledChange=\{updateLocalEchoEnabled\}/);
});

// -------------------------------------------------------------------------
// Pure logic: apply/clamp helpers shared by desktop + web-harness
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
  // Emoji / astral characters are still a single logical codepoint.
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

test('clampLocalEchoAnchor keeps the pending-text anchor inside the overlay bounds', () => {
  const bounds = { width: 400, height: 300 };
  assert.deepEqual(clampLocalEchoAnchor({ x: 200, y: 150 }, bounds), { x: 200, y: 150 });
  assert.deepEqual(clampLocalEchoAnchor({ x: -50, y: -50 }, bounds), { x: 12, y: 12 });
  assert.deepEqual(clampLocalEchoAnchor({ x: 5000, y: 5000 }, bounds), { x: 388, y: 288 });
});

// -------------------------------------------------------------------------
// Control-route wiring: gated behind the setting, zero wire changes
// -------------------------------------------------------------------------

test('the controller overlay gates all local echo rendering behind session.localEchoEnabled', () => {
  assert.match(controlRouteSource, /const localEchoEnabled = \$derived\(session\.localEchoEnabled\);/);
  assert.match(controlRouteSource, /\{#if localEchoEnabled\}/);
  assert.match(controlRouteSource, /if \(localEchoEnabled\) \{/);
});

test('local echo hooks pointerdown, wheel, and keydown SEND paths with zero new wire fields', () => {
  assert.match(controlRouteSource, /spawnEchoRipple\(event\.clientX, event\.clientY, event\.currentTarget as HTMLElement\)/);
  // #450 moved keydown delivery to a window-level listener (DOM focus no
  // longer gates it), so echo now targets the tracked overlay element
  // instead of event.currentTarget (which would be `window`).
  assert.match(controlRouteSource, /if \(controlOverlay\) handleEchoKeydown\(event, controlOverlay\);/);
  // Local echo never adds fields to the outgoing draft/send() calls -- it
  // only reads the same event that's already being sent.
  assert.doesNotMatch(controlRouteSource, /echo[A-Za-z]*:\s*(?:echo|pending)[A-Za-z]*,?\s*\n\s*(?:x|y|button|deltaX)/);
});

test('Escape is forwarded with key location; local echo clears on inactive and unmount', () => {
  // The remote-control key path (onKey and everything below it) must never
  // special-case Escape -- it is forwarded verbatim like any other key. Draw
  // mode's onDrawKey (above onKey) legitimately handles Escape to cancel a
  // text draft, so scope the check to the control-forwarding region only.
  const onKeyIndex = controlRouteSource.indexOf('function onKey(');
  assert.ok(onKeyIndex !== -1, 'onKey must exist');
  const controlKeyPath = controlRouteSource.slice(onKeyIndex);
  assert.doesNotMatch(controlKeyPath, /event\.key === 'Escape'/);
  assert.match(controlRouteSource, /location: event\.location/);

  const setActiveBlock = controlRouteSource.match(
    /__petalRemoteControlSetActive = \(value: boolean\) => \{[\s\S]*?\n {6}\};/
  );
  assert.ok(setActiveBlock, '__petalRemoteControlSetActive must exist');
  assert.match(setActiveBlock[0], /if \(!active\) clearLocalEcho\(\);/);

  const onDestroyBlock = controlRouteSource.match(/onDestroy\(\(\) => \{[\s\S]*?\n\s{2}\}\);/);
  assert.ok(onDestroyBlock, 'onDestroy must exist');
  assert.match(onDestroyBlock[0], /clearLocalEcho\(\);/);
});

test('the pending-text strip is always labeled as unconfirmed, never as real content', () => {
  assert.match(controlRouteSource, /sent, unconfirmed/);
  assert.match(controlRouteSource, /aria-hidden="true"/);
});
