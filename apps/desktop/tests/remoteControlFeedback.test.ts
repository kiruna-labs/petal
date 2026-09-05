import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { COMMANDS } from '../src/lib/ipc.ts';
import {
  REMOTE_CONTROL_REQUEST_TIMEOUT_MS,
  REMOTE_CONTROL_TIMEOUT_MESSAGE,
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
  assert.equal(remoteControlFeedbackLabel('targetUnavailable'), 'Unavailable');
  assert.equal(remoteControlFeedbackLabel('requestUnavailable'), 'Unavailable');
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
