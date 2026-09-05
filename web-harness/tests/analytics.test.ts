import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  EVENT_NAMES,
  apiKey,
  archLabel,
  classifyRemoteControl,
  commonProperties,
  commonPropertyKeys,
  consumeLeaveRequested,
  deviceChanged,
  durationBucket,
  installTestSink,
  isPermissionDeniedError,
  joinFailedFromError,
  joinFailedReasonFromError,
  meetingJoined,
  meetingLeft,
  noteAudioEnergy,
  noteLeaveRequested,
  noteRemoteControlApplied,
  noteVideoFrames,
  osLabel,
  permissionDenied,
  reconnectCountBucket,
  reconnectFailed,
  reconnectRecovered,
  shareStarted,
  uninstallTestSink,
  videoStallSource,
} from '../src/analytics.ts';

const analyticsSrc = readFileSync(new URL('../src/analytics.ts', import.meta.url), 'utf8');
const hostLedger = readFileSync(new URL('../src/remoteControlHostLedger.ts', import.meta.url), 'utf8');
const connection = readFileSync(new URL('../src/connection.ts', import.meta.url), 'utf8');
const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const tiles = readFileSync(new URL('../src/tiles.ts', import.meta.url), 'utf8');
const pipelineStats = readFileSync(new URL('../src/pipelineStats.ts', import.meta.url), 'utf8');
const pkg = readFileSync(new URL('../package.json', import.meta.url), 'utf8');

test('closed allowlist is twelve names and never posthog-js', () => {
  assert.equal(EVENT_NAMES.length, 12);
  assert.doesNotMatch(pkg, /posthog-js/);
  assert.doesNotMatch(pkg, /"posthog"/);
  assert.match(analyticsSrc, /VITE_PETAL_POSTHOG_KEY/);
  assert.match(analyticsSrc, /startsWith\('phc_'\)/);
  assert.doesNotMatch(analyticsSrc, /phc_[A-Za-z0-9]{8,}/);
  assert.match(analyticsSrc, /\$geoip_disable/);
  assert.match(analyticsSrc, /client: 'web'/);
});

test('keyless capture is a no-op outside the test sink', () => {
  uninstallTestSink();
  assert.equal(apiKey(), undefined);
  meetingJoined();
  meetingLeft();
});

test('payloads only carry allowlisted keys and client=web', () => {
  const events = installTestSink();
  try {
    meetingJoined();
    shareStarted('picker');
    permissionDenied('mic');
    assert.equal(events[0]?.name, 'meeting_joined');
    assert.equal(events[0]?.properties.client, 'web');
    for (const event of events) {
      for (const key of Object.keys(event.properties)) {
        assert.ok(
          commonPropertyKeys().includes(key) ||
            ['source', 'kind', 'reason', 'duration_bucket', 'reconnect_count_bucket', 'outcome', 'change'].includes(
              key
            ),
          `leaked property ${key}`
        );
      }
      for (const forbidden of [
        'room',
        'identity',
        'window_title',
        'sid',
        'ip',
        'dsn',
        'token',
        'device_name',
        'ua',
        'userAgent',
      ]) {
        assert.equal(event.properties[forbidden], undefined);
      }
    }
  } finally {
    uninstallTestSink();
  }
});

test('duration and reconnect buckets match the allowlist', () => {
  assert.equal(durationBucket(9_999), '0_10s');
  assert.equal(durationBucket(10_000), '10_30s');
  assert.equal(durationBucket(29_999), '10_30s');
  assert.equal(durationBucket(30_000), '30_120s');
  assert.equal(durationBucket(120_000), '120s_plus');
  assert.equal(reconnectCountBucket(0), '0');
  assert.equal(reconnectCountBucket(1), '1');
  assert.equal(reconnectCountBucket(4), '2_4');
  assert.equal(reconnectCountBucket(5), '5_plus');
});

test('join-failed reasons map timeout and 4xx token errors', () => {
  assert.equal(joinFailedReasonFromError(new Error('token request timed out')), 'timeout');
  assert.equal(joinFailedReasonFromError(new Error('token request failed (401)')), 'token');
  assert.equal(joinFailedReasonFromError(new Error('Failed to fetch')), 'network');
  const events = installTestSink();
  try {
    joinFailedFromError(new Error('token request timed out'));
    assert.equal(events[0]?.properties.reason, 'timeout');
  } finally {
    uninstallTestSink();
  }
});

test('meeting leave carries reconnect count; device changes require a meeting', () => {
  const events = installTestSink();
  try {
    deviceChanged('mic', 'switched');
    meetingJoined();
    reconnectRecovered();
    deviceChanged('mic', 'switched');
    meetingLeft();
    assert.deepEqual(
      events.map((event) => event.name),
      ['meeting_joined', 'reconnect', 'device_changed', 'meeting_left']
    );
    assert.equal(events[3]?.properties.reconnect_count_bucket, '1');
    assert.equal(events[2]?.properties.kind, 'mic');
  } finally {
    uninstallTestSink();
  }
});

test('user leave is not a failed reconnect', () => {
  const events = installTestSink();
  try {
    meetingJoined();
    noteLeaveRequested();
    const userLeft = consumeLeaveRequested();
    if (!userLeft) reconnectFailed();
    meetingLeft();
    assert.deepEqual(
      events.map((event) => event.name),
      ['meeting_joined', 'meeting_left']
    );
  } finally {
    uninstallTestSink();
  }
});

test('remote-control coalescer matches native idle windows and ignores moves', () => {
  assert.equal(classifyRemoteControl({ kind: 'pointer', action: 'move' }), null);
  const events = installTestSink();
  try {
    const t0 = 1_000;
    assert.equal(noteRemoteControlApplied({ kind: 'pointer', action: 'click' }, t0), 'click');
    assert.equal(noteRemoteControlApplied({ kind: 'pointer', action: 'click' }, t0 + 20), 'click');
    assert.equal(noteRemoteControlApplied({ kind: 'key' }, t0), 'type');
    assert.equal(noteRemoteControlApplied({ kind: 'key' }, t0 + 200), null);
    assert.equal(noteRemoteControlApplied({ kind: 'key' }, t0 + 1300), 'type');
    assert.equal(noteRemoteControlApplied({ kind: 'wheel' }, t0), 'scroll');
    assert.equal(noteRemoteControlApplied({ kind: 'wheel' }, t0 + 100), null);
    assert.equal(noteRemoteControlApplied({ kind: 'text' }, t0), 'paste');
    assert.deepEqual(
      events.map((event) => event.properties.kind),
      ['click', 'click', 'type', 'type', 'scroll', 'paste']
    );
  } finally {
    uninstallTestSink();
  }
});

test('video stall fires once per alarm after 10s without a decoded-frame advance', () => {
  const events = installTestSink();
  try {
    meetingJoined();
    noteVideoFrames('cam', 10, 'gallery', 0);
    noteVideoFrames('cam', 10, 'gallery', 9_999);
    noteVideoFrames('cam', 10, 'gallery', 10_000);
    noteVideoFrames('cam', 10, 'gallery', 11_000);
    noteVideoFrames('cam', 12, 'gallery', 12_000);
    noteVideoFrames('cam', 12, 'gallery', 22_000);
    assert.deepEqual(
      events.filter((event) => event.name === 'remote_video_stalled').map((event) => event.properties),
      [
        // No duration_bucket: it was hardcoded '0_10s' on every stall, so the
        // dimension was fabricated. Asserting its absence keeps it from
        // creeping back without a real duration behind it.
        { ...commonProperties(), source: 'gallery' },
        { ...commonProperties(), source: 'gallery' },
      ]
    );
  } finally {
    uninstallTestSink();
  }
});

test('audio silence fires once per unmuted 10s gap and ignores mute', () => {
  const events = installTestSink();
  try {
    meetingJoined();
    noteAudioEnergy('a', false, true, 0);
    noteAudioEnergy('a', false, false, 1_000);
    noteAudioEnergy('a', false, false, 11_000);
    noteAudioEnergy('a', false, false, 12_000);
    noteAudioEnergy('a', true, false, 13_000);
    assert.equal(events.filter((event) => event.name === 'remote_audio_silent').length, 1);
    assert.equal(events.at(-1)?.properties.duration_bucket, '10_30s');
  } finally {
    uninstallTestSink();
  }
});

test('os and arch labels never echo the user agent string', () => {
  assert.equal(osLabel('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', 'MacIntel'), 'macos');
  assert.equal(osLabel('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32'), 'windows');
  assert.equal(archLabel('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)'), 'unknown');
  assert.equal(videoStallSource('stats-frame-starvation'), 'stats');
  assert.equal(videoStallSource('gallery-bridge-freeze-watchdog'), 'gallery');
  assert.ok(isPermissionDeniedError({ name: 'NotAllowedError' }));
  assert.equal(isPermissionDeniedError({ name: 'NotFoundError' }), false);
});

test('web call sites exist; host emulator and hold-last-frame are not event sources', () => {
  assert.match(connection, /meetingJoined\(/);
  assert.match(connection, /meetingLeft\(/);
  assert.match(connection, /joinFailedFromError\(/);
  assert.match(connection, /reconnectRecovered\(/);
  assert.match(connection, /reconnectFailed\(/);
  assert.match(connection, /noteAudioEnergy|startRemoteAudioSilenceWatchdog/);
  assert.match(controls, /shareStarted\('picker'\)/);
  assert.match(controls, /shareStarted\('window'\)/);
  assert.match(controls, /shareStopped\('user'\)/);
  assert.match(controls, /permissionDenied\('mic'\)/);
  assert.match(controls, /permissionDenied\('camera'\)/);
  assert.match(controls, /noteLeaveRequested\(/);
  assert.match(tiles, /noteVideoFrames\(/);
  assert.match(pipelineStats, /noteVideoFrames\(/);
  assert.doesNotMatch(hostLedger, /noteRemoteControlApplied|remote_control_input/);
  assert.match(analyticsSrc, /VIDEO_STALL_MS = 10_000/);
  assert.doesNotMatch(analyticsSrc, /HOLD_STALL_MS/);
});
