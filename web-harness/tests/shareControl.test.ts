import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { supportsScreenShare } from '../src/controls.ts';

const index = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const style = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const sharedStyle = readFileSync(new URL('../../shared/ui/meeting-controls.css', import.meta.url), 'utf8');
const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const uiHelpers = readFileSync(new URL('../src/ui/uiHelpers.ts', import.meta.url), 'utf8');

test('web harness share control is a single button, matching the native Gallery ControlButton (not a segmented Sharing/Stop pill)', () => {
  assert.match(index, /id="ctl-share" class="control-button"/);
  assert.doesNotMatch(index, /share-mode-switcher|share-active-indicator|share-mode-segment|share-identity-dot/);
  assert.doesNotMatch(style, /\.share-mode-switcher|\.share-active-indicator|\.share-mode-segment|\.share-identity-dot/);
  // Same screenshare glyph as the native ControlButton.svelte.
  assert.match(index, /id="ctl-share"[\s\S]*<rect x="3" y="4" width="18" height="13" rx="2"><\/rect>[\s\S]*<path d="M8 21h8M12 17v4">/);
});

test('control-bar uses stable labels, attached device options, and a top-level Draw button', () => {
  for (const label of ['Mic', 'Camera', 'Share', 'Invite', 'Draw', 'Leave']) {
    assert.match(index, new RegExp(`meeting-control-label[^>]*>${label}<`));
  }
  assert.match(index, /id="ctl-audio-options" class="meeting-split-options"/);
  assert.match(index, /id="ctl-video-options" class="meeting-split-options"/);
  assert.match(index, /id="ctl-draw" class="control-button"/);
  assert.match(sharedStyle, /\.meeting-split-options\s*\{/);
  assert.match(style, /\.controlbar\s*\{[\s\S]*flex-wrap: nowrap;/);
  assert.match(style, /\.controls-left\s*\{[\s\S]*flex-wrap: nowrap;/);
});

test('web harness keeps the existing ctl-share behavior and identity color state', () => {
  assert.match(controls, /ctlShare\.addEventListener\('click'/);
  assert.match(uiHelpers, /options\.ctlShare\.classList\.toggle\('live', on\)/);
  assert.match(uiHelpers, /options\.ctlShare\.style\.setProperty\('--control-live-bg', colorForIdentity\(trimmedIdentity, paletteIndex\)\)/);
  assert.match(uiHelpers, /options\.ctlShare\.setAttribute\('aria-label', on \? 'Stop sharing your screen' : 'Share your screen'\)/);
  assert.match(style, /\.control-button\.live\s*\{[\s\S]*var\(--control-live-bg/);
});

test('Share is offered only where the browser has getDisplayMedia (#240)', () => {
  assert.equal(supportsScreenShare({ getDisplayMedia: () => Promise.reject(new Error('never called')) }), true);
  // Chrome for Android / iPhone Safari: mediaDevices exists, getDisplayMedia does not.
  assert.equal(supportsScreenShare({}), false);
  // Node, like a page outside a secure context, has no navigator.mediaDevices.
  assert.equal(supportsScreenShare(), false);
});

test('an unsupported browser is told so before any picker is attempted (#240)', () => {
  // Source-level because the click handler returns before this point with no
  // live Room. The guard must come before the picker log and the capture
  // call, and must never claim a cancellation: nothing was offered.
  const startShare = controls.slice(controls.indexOf("ctlShare.addEventListener('click'"));
  const guardAt = startShare.indexOf('if (!supportsScreenShare()) {');
  assert.ok(guardAt > 0, 'the start-share branch must check supportsScreenShare()');
  assert.ok(guardAt < startShare.indexOf('watch for the browser picker'));
  assert.ok(guardAt < startShare.indexOf('onShareStartIntent()'));
  assert.ok(guardAt < startShare.indexOf('getDisplayMedia('));
  const guard = startShare.slice(guardAt, startShare.indexOf('return;', guardAt));
  assert.match(guard, /showToast\("Screen sharing isn't available in this browser"\)/);
  assert.doesNotMatch(guard, /cancel/i);
});
