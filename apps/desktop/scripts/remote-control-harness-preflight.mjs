#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const desktopDir = path.resolve(scriptDir, '..');
const repoRoot = path.resolve(desktopDir, '../..');
const contractPath = path.join(repoRoot, 'contracts/petal-contracts.json');
const remoteControlPath = path.join(desktopDir, 'src-tauri/src/remote_control.rs');
const webContractsPath = path.join(repoRoot, 'web-harness/tests/contracts.test.ts');
const webRemoteControlPath = path.join(repoRoot, 'web-harness/tests/remoteControl.test.ts');
const webHarnessApiPath = path.join(repoRoot, 'web-harness/src/harnessApi.ts');
const webPhotonDecoderPath = path.join(repoRoot, 'web-harness/src/remoteControlPhoton.ts');
const webPhotonTestPath = path.join(repoRoot, 'web-harness/tests/remoteControlPhoton.test.ts');
const localLoopbackPath = path.join(desktopDir, 'scripts/remote-control-local-loopback.mjs');
const liveScenarioPath = path.join(desktopDir, 'scripts/remote-control-scenario.mjs');
const photonSentinelPath = path.join(desktopDir, 'scripts/remote-control-photon-sentinel.swift');
const photonMetricsPath = path.join(desktopDir, 'scripts/remote-control-photon-metrics.mjs');

const rawArgs = process.argv.slice(2);
const allowedArgs = new Set(['--check-only', '--skip-swift-typecheck', '--help', '-h']);

function usage() {
  console.log(`Remote-control harness preflight

Usage:
  node apps/desktop/scripts/remote-control-harness-preflight.mjs --check-only
  node apps/desktop/scripts/remote-control-harness-preflight.mjs --check-only --skip-swift-typecheck

The default check-only path runs the portable contract/inventory checks and the
required Swift/AppKit photon-sentinel typecheck. --skip-swift-typecheck is only
for the explicit portable CI path: it still runs every portable check and
reports the Swift sentinel as DEFERRED to the named macOS CI gate.`);
}

const unknownArgs = rawArgs.filter((arg) => !allowedArgs.has(arg));
if (unknownArgs.length > 0) {
  throw new Error(`unknown argument(s): ${unknownArgs.join(', ')}; pass --help for usage`);
}
if (new Set(rawArgs).size !== rawArgs.length) {
  throw new Error('duplicate arguments are not allowed; pass --help for usage');
}
if (rawArgs.includes('--help') || rawArgs.includes('-h')) {
  if (rawArgs.length !== 1) {
    throw new Error('--help cannot be combined with other arguments');
  }
  usage();
  process.exit(0);
}

const checkOnly = rawArgs.includes('--check-only');
const skipSwiftTypecheck = rawArgs.includes('--skip-swift-typecheck');
if (skipSwiftTypecheck && !checkOnly) {
  throw new Error('--skip-swift-typecheck is only valid with --check-only');
}

function section(title) {
  console.log(`\n==> ${title}`);
}

function pass(message) {
  console.log(`ok ${message}`);
}

function fail(message) {
  throw new Error(message);
}

function readJson(file) {
  return JSON.parse(readFileSync(file, 'utf8'));
}

function assertDeepEqual(actual, expected, label) {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  if (actualJson !== expectedJson) {
    fail(`${label} drifted\nactual:   ${actualJson}\nexpected: ${expectedJson}`);
  }
}

function requireMarkers(file, markers) {
  const source = readFileSync(file, 'utf8');
  const missing = markers.filter((marker) => !source.includes(marker));
  if (missing.length > 0) {
    fail(`${path.relative(repoRoot, file)} is missing harness marker(s): ${missing.join(', ')}`);
  }
  pass(`${path.relative(repoRoot, file)} carries ${markers.length} expected harness markers`);
}

function run(command, args, cwd, env = {}) {
  console.log(`$ ${command} ${args.join(' ')}`);
  execFileSync(command, args, {
    cwd,
    env: { ...process.env, ...env },
    stdio: 'inherit'
  });
}

section('Contract fixture');
const fixture = readJson(contractPath);
assertDeepEqual(fixture.topics.remoteControl, 'petal.remote-control', 'remote-control topic');
assertDeepEqual(
  fixture.topics.remoteClipboardText,
  'petal.remote-control.clipboard-text',
  'remote clipboard stream topic'
);
assertDeepEqual(
  fixture.remoteClipboardStreams,
  {
    topic: 'petal.remote-control.clipboard-text',
    mimeType: 'text/plain; charset=utf-8',
    directions: ['copyResponse', 'paste'],
    attributes: ['direction', 'grantToken', 'operationId', 'windowId'],
    operationIdHexLength: 32,
    maxBytes: 1048576,
    reliability: 'reliable',
    destination: 'oneAuthenticatedParticipant',
    successSignals: { copyResponse: 'targetedTextStreamOnly', paste: 'none' },
    textRules: [
      'nonempty',
      'validUtf8',
      'noNul',
      'plainTextOnly',
      'rejectRecognizedFileClipboardFormats',
      'rejectOversize'
    ]
  },
  'remote clipboard stream contract'
);
assertDeepEqual(
  fixture.remoteClipboardMessages.map((entry) => [entry.name, entry.reliable]),
  [['copy-request', true], ['copy-request-capable-window', true]],
  'remote clipboard request matrix'
);
for (const entry of fixture.remoteClipboardMessages) {
  assertDeepEqual(
    Object.keys(entry.message).sort(),
    entry.fields,
    `${entry.name} field list`
  );
}

const expectedRemoteControl = [
  ['request', true],
  ['release', true],
  ['status', true],
  ['status-request-unavailable', true],
  ['pointer-move', false],
  ['pointer-down', true],
  ['pointer-up', true],
  ['pointer-click', true],
  ['pointer-double-click', true],
  ['wheel', false],
  ['key', true],
  ['text', true],
  ['pointer-click-v2-canonical-fingerprint', true],
  ['result-applied-v2', true],
  ['result-replay-failed-v2', true],
  // Windows-oriented vocabulary (targetKind / capability negotiation) added
  // with the platform-neutral core in b5833159. This list is hand-maintained,
  // so a contract fixture added without a matching entry here fails the gate
  // with "matrix drifted" -- append here in the same commit.
  ['request-capable-window', true],
  ['status-active-capable-window', true],
  ['pointer-click-capable-window', true],
  ['result-submitted-capable-display', true],
  ['status-controller-upgrade-required', true],
  ['status-awaiting-consent', true],
  ['status-denied', true]
];
assertDeepEqual(
  fixture.remoteControlMessages.map((entry) => [entry.name, entry.reliable]),
  expectedRemoteControl,
  'remote-control variant/reliability matrix'
);
assertDeepEqual(
  fixture.remoteControlMessages.slice(2, 4).map((entry) => [entry.name, entry.reliable]),
  [
    ['status', true],
    ['status-request-unavailable', true]
  ],
  'reliable status-request-unavailable canonical placement'
);

for (const entry of fixture.remoteControlMessages) {
  assertDeepEqual(Object.keys(entry.message).sort(), entry.fields, `${entry.name} field list`);
  if (entry.message.modifiers) {
    assertDeepEqual(
      Object.keys(entry.message.modifiers).sort(),
      ['alt', 'ctrl', 'meta', 'shift'],
      `${entry.name} modifier fields`
    );
  }
}
pass('all v1 and v2 variants, field names, modifiers, and reliability are pinned');

section('Harness inventory');
requireMarkers(remoteControlPath, [
  'trait InputSink',
  'struct CGEventSink',
  'struct RecordingSink',
  'recording_sink_replays_command_key_without_unicode_fallback',
  'recording_sink_keeps_navigation_function_and_unknown_keys_off_text_path',
  'recording_sink_replays_scroll_modes_with_horizontal_axis_and_vertical_sign',
  'recording_sink_replays_right_drag_from_buttons_bitmask',
  'recording_sink_replays_clamped_pointer_coordinates',
  'pointer_message_fields_match_shared_contract_fixture',
  'remote_control_reliability_keeps_high_rate_streams_unreliable',
  'stale_unreliable_sequences_are_dropped_per_stream',
  'pointer_revoke_synthesizes_matching_button_release',
  'key_revoke_synthesizes_matching_key_release'
]);
requireMarkers(webContractsPath, [
  'remote-control topic and all-variant JSON field names are pinned',
  'remote-control fixture pins representative variant payloads'
]);
requireMarkers(webRemoteControlPath, [
  'remoteControlPublishOptions sends motion and wheel streams unreliably',
  'remoteControlModifiers exposes the stable modifier field names',
  'remote-control live scenario separates case duration from measured target-observation latency',
  'remote-control harness records native host status packets'
]);
requireMarkers(webHarnessApiPath, [
  'statusMetrics.push',
  'handleRemoteControlPayload'
]);
requireMarkers(localLoopbackPath, [
  'Remote-control local-loopback harness',
  'PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS',
  '--press-to-photon',
  'CGEventPostToPid'
]);
requireMarkers(liveScenarioPath, [
  'PETAL_REMOTE_CONTROL_INPUT_BUDGET_MS',
  'targetObservationLatencyMs',
  'caseDurationMs',
  'measureTargetObservation'
]);
requireMarkers(webPhotonDecoderPath, [
  'decodePhotonSentinelFrame',
  'calibrationMatches',
  'grayBlocksToFrameCounter'
]);
requireMarkers(webPhotonTestPath, [
  'recovers static generations at full and half resolution',
  'rejects ambiguous bits and missing calibration',
  'rejects stale frames and handles wraparound'
]);
requireMarkers(photonSentinelPath, [
  'PETAL_RC_PHOTON_SENTINEL_READY',
  'controlTextDidChange',
  'remoteClick'
]);
requireMarkers(photonMetricsPath, [
  'nearestRankPercentile',
  'summarizePhotonSamples',
  'pressToEstimatedPhotonMs'
]);
if (/\blatencyMs\b/.test(readFileSync(liveScenarioPath, 'utf8'))) {
  fail('remote-control live scenario must not label whole-case duration as latencyMs');
}
section('Swift/AppKit photon sentinel');
if (skipSwiftTypecheck) {
  console.log('DEFERRED Swift/AppKit sentinel: exercised by the named macOS CI gate');
} else {
  run('xcrun', ['swiftc', '-typecheck', photonSentinelPath], repoRoot, {
    SWIFT_MODULE_CACHE_PATH: '/tmp/petal-rc-photon-swift-cache',
    CLANG_MODULE_CACHE_PATH: '/tmp/petal-rc-photon-clang-cache'
  });
}

if (checkOnly) {
  section('Headless tests');
  console.log('skipped by --check-only');
} else {
  section('Headless tests');
  const dyldFallback = '/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx';
  const testEnv = existsSync(dyldFallback)
    ? { DYLD_FALLBACK_LIBRARY_PATH: dyldFallback }
    : {};
  run('cargo', ['test', '--lib', 'remote_control', '--locked'], path.join(desktopDir, 'src-tauri'), testEnv);
  run('npm', ['test'], path.join(repoRoot, 'web-harness'));
}

section('Live-only remainder');
console.log('scripted local Mac + Accessibility: apps/desktop/scripts/remote-control-scenario.mjs drives web-controller -> native-host -> TextEdit and enforces named target-observation latency budgets');
console.log('local press-to-photon: add --press-to-photon to drive the AppKit sentinel and gate browser estimated-display p95');
console.log('packaged local loopback: apps/desktop/scripts/remote-control-local-loopback.mjs --live prints setup, thresholds, and a pass/fail scorecard');
console.log('needs live native-controller -> native-host transport E2E');
console.log('needs expanded timing harness: move/wheel flood, focus/scroll/paste/drag scorecard, disconnect/consent chaos');

console.log('\nremote-control harness preflight passed');
