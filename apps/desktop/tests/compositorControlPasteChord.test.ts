import assert from 'node:assert/strict';
import test from 'node:test';

import {
  isPasteChord,
  remoteClipboardChord
} from '../src/lib/data/compositorControl.ts';

const bareModifiers = { metaKey: false, ctrlKey: false, altKey: false, shiftKey: false };

test('isPasteChord matches bare Cmd+V by logical key, case-insensitively', () => {
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'v', code: 'KeyV' }), true);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'V', code: 'KeyV' }), true);
});

test('isPasteChord falls back to the physical code when the logical key is empty', () => {
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: '', code: 'KeyV' }), true);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, code: 'KeyV' }), true);
});

test('isPasteChord prefers the logical key over the physical code (non-US layouts)', () => {
  // On AZERTY physical KeyV differs from logical 'v'; only the logical key
  // should decide the match once it is present -- mirrors classify_text_
  // shortcut_prefers_logical_key_over_physical_code in remote_control.rs.
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'j', code: 'KeyV' }), false);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'v', code: 'KeyDot' }), true);
});

test('isPasteChord rejects Cmd+V with any extra modifier', () => {
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: true, altKey: false, shiftKey: false, key: 'v' }), false);
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: false, altKey: true, shiftKey: false, key: 'v' }), false);
  assert.equal(isPasteChord({ metaKey: true, ctrlKey: false, altKey: false, shiftKey: true, key: 'v' }), false);
});

test('isPasteChord rejects plain V and other letters', () => {
  assert.equal(isPasteChord({ ...bareModifiers, key: 'v' }), false);
  assert.equal(isPasteChord({ ...bareModifiers, metaKey: true, key: 'c', code: 'KeyC' }), false);
});

test('remoteClipboardChord recognizes bare macOS Copy and Paste', () => {
  assert.equal(remoteClipboardChord({ ...bareModifiers, metaKey: true, key: 'c' }, 'macos'), 'copy');
  assert.equal(remoteClipboardChord({ ...bareModifiers, metaKey: true, key: 'v' }, 'macos'), 'paste');
  assert.equal(remoteClipboardChord({ ...bareModifiers, metaKey: true, key: 'j', code: 'KeyC' }, 'macos'), null);
});

test('remoteClipboardChord recognizes bare Windows Ctrl+C/V', () => {
  assert.equal(remoteClipboardChord({ ...bareModifiers, ctrlKey: true, key: 'c' }, 'windows'), 'copy');
  assert.equal(remoteClipboardChord({ ...bareModifiers, ctrlKey: true, key: 'v' }, 'windows'), 'paste');
  assert.equal(remoteClipboardChord({ ...bareModifiers, ctrlKey: true, key: '', code: 'KeyV' }, 'windows'), 'paste');
});

test('remoteClipboardChord rejects extra modifiers and wrong platform', () => {
  assert.equal(remoteClipboardChord({ metaKey: true, ctrlKey: false, altKey: true, shiftKey: false, key: 'c' }, 'macos'), null);
  assert.equal(remoteClipboardChord({ metaKey: true, ctrlKey: false, altKey: false, shiftKey: true, key: 'v' }, 'macos'), null);
  assert.equal(remoteClipboardChord({ metaKey: true, ctrlKey: true, altKey: false, shiftKey: false, key: 'v' }, 'macos'), null);
  assert.equal(remoteClipboardChord({ ...bareModifiers, metaKey: true, key: 'v' }, 'windows'), null);
});
