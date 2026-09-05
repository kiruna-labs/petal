import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const route = readFileSync(new URL('../src/routes/compositor/control/+page.svelte', import.meta.url), 'utf8');
const control = readFileSync(new URL('../src/lib/data/compositorControl.ts', import.meta.url), 'utf8');
const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');
const lib = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const capabilities = readFileSync(
  new URL('../src-tauri/capabilities/default.json', import.meta.url),
  'utf8'
);
const remoteControl = readFileSync(
  new URL('../src-tauri/src/remote_control.rs', import.meta.url),
  'utf8'
);

test('native control overlay routes Copy and Paste through native commands', () => {
  assert.match(route, /remoteClipboardChord/);
  assert.match(route, /COMMANDS\.remoteClipboardCopy/);
  assert.match(route, /COMMANDS\.remoteClipboardPaste/);
  assert.match(route, /function invokeClipboardOperation/);
  assert.match(route, /pendingClipboardModifier/);
  assert.match(route, /clipboardModifierConsumed/);
  assert.doesNotMatch(route, /plugin-clipboard-manager/);
  assert.doesNotMatch(route, /readClipboardText/);
  assert.doesNotMatch(route, /function pasteControllerClipboard/);
});

test('generic text input remains separate from native clipboard Paste', () => {
  assert.match(route, /function sendComposedText/);
  assert.match(route, /kind: 'text'/);
  assert.match(control, /export function remoteClipboardChord/);
  assert.match(control, /export function isPasteChord/);
});

test('clipboard streams declare their exact byte length', () => {
  assert.match(remoteControl, /total_length: Some\(bytes\.len\(\) as u64\)/);
  assert.match(remoteControl, /let declared_length = usize::try_from\(info\.total_length\?\)/);
});

test('clipboard commands are registered on both native command surfaces', () => {
  assert.match(ipc, /remoteClipboardCopy: 'remote_clipboard_copy'/);
  assert.match(ipc, /remoteClipboardPaste: 'remote_clipboard_paste'/);
  assert.match(lib, /remote_control::remote_clipboard_copy/);
  assert.match(lib, /remote_control::remote_clipboard_paste/);
  assert.equal(
    (lib.match(/remote_control::remote_clipboard_copy/g) ?? []).length,
    2,
    'macOS and Windows handlers must both register Copy'
  );
  assert.equal(
    (lib.match(/remote_control::remote_clipboard_paste/g) ?? []).length,
    2,
    'macOS and Windows handlers must both register Paste'
  );
});

test('remote-window webviews no longer receive clipboard read permission', () => {
  assert.doesNotMatch(capabilities, /clipboard-manager:allow-read-text/);
});

test('Windows control and overlay webviews are in the native capability scope', () => {
  assert.match(capabilities, /"petal-control-\*"/);
  assert.match(capabilities, /"petal-pointer-\*"/);
  assert.match(capabilities, /"petal-sharer-pointer-\*"/);
});
