import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { COMMANDS } from '../src/lib/ipc.ts';
import {
  HEADER_STATUS_TEXT_MIN_WIDTH,
  REMOTE_CONTROL_REQUEST_TIMEOUT_MS,
  REMOTE_CONTROL_TIMEOUT_MESSAGE,
  controlBannerShowsLabel,
  remoteControlFeedbackLabel,
  remoteControlFeedbackTitle,
  remoteControlStatusEffect
} from '../src/lib/remoteControlFeedback.ts';

const surfaceRoute = readFileSync(
  new URL('../src/routes/compositor/surface/+page.svelte', import.meta.url),
  'utf8'
);
const headerComponent = readFileSync(
  new URL('../src/lib/components/RemoteWindowHeader.svelte', import.meta.url),
  'utf8'
);
const remoteControlSource = readFileSync(
  new URL('../src-tauri/src/remote_control.rs', import.meta.url),
  'utf8'
);
const controlRoute = readFileSync(
  new URL('../src/routes/compositor/control/+page.svelte', import.meta.url),
  'utf8'
);
const libSource = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');

test('remote control accessibility denial is visible in the controller header', () => {
  assert.equal(remoteControlFeedbackLabel('accessibilityDenied'), 'Needs access');
  assert.equal(
    remoteControlFeedbackTitle('accessibilityDenied', 'Grant Accessibility on the shared Mac.'),
    'Grant Accessibility on the shared Mac.'
  );
  assert.match(headerComponent, /remoteControlFeedbackLabel\(remoteControlStatus\)/);
  assert.match(headerComponent, /class:warning=\{remoteControlFeedbackWarning\}/);
});

test('remote control request prerequisites use neutral unavailable feedback', () => {
  // #407: the two used to share "Unavailable"; they mean different things.
  assert.equal(remoteControlFeedbackLabel('targetUnavailable'), 'Window out of reach');
  assert.equal(remoteControlFeedbackLabel('requestUnavailable'), 'Control unavailable');
  assert.match(remoteControlSource, /status: "requestUnavailable"[\s\S]*not being shared/);
  assert.match(remoteControlSource, /"requestUnavailable",[\s\S]*requester is not in this meeting/);
  assert.match(headerComponent, /remoteControlStatus !== 'requestUnavailable'/);
  assert.match(headerComponent, /class:paused=\{\(!!remoteControlFeedback && !remoteControlFeedbackWarning\)/);
  assert.match(headerComponent, /color:\s*var\(--warning\)/);
});

test('active Windows operation feedback does not terminate remote control', () => {
  for (const status of [
    'accessibilityDenied',
    'requestFailed',
    'targetPaused',
    'targetUnavailable',
    'notForeground',
    'occluded',
    'integrityBlocked',
    'secureField',
    'unsupportedRoute',
    'staleShareInstance',
    'injectionTimeout'
  ] as const) {
    assert.equal(remoteControlStatusEffect(status), 'feedback', status);
  }
  assert.equal(remoteControlStatusEffect('active'), 'activate');
  assert.equal(remoteControlStatusEffect('stopped'), 'terminate');
  assert.equal(remoteControlStatusEffect('disabled'), 'terminate');
  assert.equal(remoteControlFeedbackLabel('requestFailed'), 'Input ignored');
  assert.match(surfaceRoute, /remoteControlActive && effect === 'feedback'/);
});

test('remote control request timeout clears Requesting and logs through native IPC', () => {
  assert.equal(REMOTE_CONTROL_REQUEST_TIMEOUT_MS, 8000);
  assert.match(REMOTE_CONTROL_TIMEOUT_MESSAGE, /timed out/);
  assert.equal(COMMANDS.remoteControlRequestTimedOut, 'remote_control_request_timed_out');
  assert.match(surfaceRoute, /setTimeout\(\(\) => \{/);
  assert.match(surfaceRoute, /setRemoteControlFeedback\('requestFailed', timeoutMessage\)/);
  assert.match(surfaceRoute, /timeoutMessage: string = REMOTE_CONTROL_TIMEOUT_MESSAGE/);
  assert.match(surfaceRoute, /invoke\(COMMANDS\.remoteControlRequestTimedOut, \{ windowId, ownerIdentity \}\)/);
  assert.match(remoteControlSource, /controller timeout waiting for active status/);
  assert.match(libSource, /remote_control::remote_control_request_timed_out/);
});

test('transient feedback bypasses the persistent lifecycle latch in Rust', () => {
  assert.match(remoteControlSource, /is_transient_feedback_status/);
  assert.match(
    remoteControlSource,
    /!matches!\(status, "active" \| "stopped" \| "disabled"\)/
  );
  assert.match(
    remoteControlSource,
    /Transient feedback bypasses the permanent lifecycle latch/
  );
});

test('control overlay renders a prominent pointer-transparent feedback banner', () => {
  assert.match(controlRoute, /control-feedback-banner/);
  assert.match(controlRoute, /pointer-events: none/);
  assert.match(controlRoute, /FEEDBACK_BANNER_MS = 3000/);
  assert.match(controlRoute, /showFeedback\(event\.payload\)/);
  // replace-don't-stack: one banner, one restarted timer, cleared on teardown.
  assert.match(controlRoute, /clearFeedback\(\)/);
  assert.match(controlRoute, /feedbackTimer = setTimeout/);
});

test('undeliverable-input feedback is plain language with a next step (#407)', () => {
  const engineering = 'Remote control input could not be delivered to the shared window.';
  const detail = remoteControlFeedbackTitle('targetUnavailable', engineering);
  assert.notEqual(detail, engineering);
  assert.match(detail ?? '', /Ask the sharer/);
  assert.match(remoteControlFeedbackTitle('notForeground', null) ?? '', /Ask the sharer to click into it/);
  assert.match(remoteControlFeedbackTitle('occluded', null) ?? '', /Ask the sharer/);
  // Statuses whose native message is already the right copy keep it.
  assert.equal(
    remoteControlFeedbackTitle('requestUnavailable', 'Remote control is unavailable for this share.'),
    'Remote control is unavailable for this share.'
  );
  for (const status of ['targetUnavailable', 'requestUnavailable', 'notForeground', 'occluded'] as const) {
    assert.ok((remoteControlFeedbackLabel(status) ?? '').length <= 20, `${status} label fits the chip`);
  }
});

test('the feedback banner never repeats a label the header chip already shows (#407)', () => {
  // The header collapses its chip text at exactly this container width.
  assert.equal(HEADER_STATUS_TEXT_MIN_WIDTH, 720);
  assert.match(
    headerComponent,
    /@container \(max-width: 720px\) \{[\s\S]*?\.status-chip-text \{[\s\S]*?clip: rect\(0 0 0 0\);/
  );
  assert.equal(controlBannerShowsLabel(1200), false);
  assert.equal(controlBannerShowsLabel(721), false);
  assert.equal(controlBannerShowsLabel(720), true);
  assert.equal(controlBannerShowsLabel(480), true);
  assert.match(controlRoute, /<svelte:window bind:innerWidth=\{overlayWidth\} \/>/);
  assert.match(controlRoute, /\{#if feedbackShowsLabel \|\| !feedbackTitle\}/);
});

test('the feedback banner uses design tokens and stays clear of the shared app top chrome (#407)', () => {
  const rule = controlRoute.match(/\.control-feedback-banner \{[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.ok(rule, 'banner rule found');
  assert.match(rule, /bottom: 16px;/);
  assert.doesNotMatch(rule, /\btop:/);
  assert.match(rule, /background: var\(--surface-raised\);/);
  assert.match(rule, /border: 1px solid var\(--hairline-strong\);/);
  assert.match(rule, /border-radius: var\(--radius-popover\);/);
  assert.doesNotMatch(rule, /rgba\(|\b\d+px\/|font: 600/);
  const detail = controlRoute.match(/\.control-feedback-detail \{[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(detail, /color: var\(--text-soft\);/);
  assert.match(detail, /font-size: var\(--text-micro\);/);
  assert.doesNotMatch(detail, /--warning|currentColor/);
});

test('the header status chip stays on one line with an ellipsis (#407)', () => {
  const chip = headerComponent.match(/\n  \.status-chip \{[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(chip, /height: 24px;/);
  assert.match(chip, /align-items: center;/);
  assert.match(chip, /white-space: nowrap;/);
  const text = headerComponent.match(/\n  \.status-chip-text \{[\s\S]*?\n  \}/)?.[0] ?? '';
  assert.match(text, /text-overflow: ellipsis;/);
  assert.match(text, /white-space: nowrap;/);
});
