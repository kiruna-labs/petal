import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const desktop = join(dirname(fileURLToPath(import.meta.url)), '..');
const root = join(desktop, '..', '..');
const analytics = readFileSync(
  join(desktop, 'src-tauri', 'src', 'analytics.rs'),
  'utf8'
);
const allowlist = readFileSync(
  join(root, 'docs', 'POSTHOG_EVENT_ALLOWLIST.md'),
  'utf8'
);

const SHARED_EVENTS = [
  'meeting_joined',
  'meeting_left',
  'join_failed',
  'share_started',
  'share_stopped',
  'remote_audio_silent',
  'remote_video_stalled',
  'capture_restarted',
  'reconnect',
  'permission_denied',
  'remote_control_input',
  'device_changed'
];

// #872: native-only — the sharer-side drawing overlay that captures the
// cursor exists only in the desktop app, so the web client never emits it.
// #370: the latency summary is native-only until the web producer lands (#375).
const NATIVE_ONLY_EVENTS = ['annotation_toggled', 'remote_control_latency_summary'];

test('allowlist and analytics.rs lock the same fourteen event names', () => {
  for (const name of [...SHARED_EVENTS, ...NATIVE_ONLY_EVENTS]) {
    assert.match(allowlist, new RegExp(`\`${name}\``));
    assert.match(analytics, new RegExp(`"${name}"`));
  }
  assert.match(analytics, /const EVENT_NAMES: \[&str; 14\]/);
  assert.match(analytics, /starts_with\("phc_"\)/);
  assert.match(analytics, /"client"/);
  assert.match(analytics, /"native"/);
  assert.doesNotMatch(analytics, /phc_[A-Za-z0-9]{8,}/);
  assert.doesNotMatch(allowlist, /not implemented/);
});

test('desktop and web analytics are host-side capture: no posthog-js', () => {
  const pkg = readFileSync(join(desktop, 'package.json'), 'utf8');
  assert.doesNotMatch(pkg, /posthog-js/);
  const harnessPkg = readFileSync(join(root, 'web-harness', 'package.json'), 'utf8');
  assert.doesNotMatch(harnessPkg, /posthog-js/);
  const webAnalytics = readFileSync(join(root, 'web-harness', 'src', 'analytics.ts'), 'utf8');
  assert.match(webAnalytics, /VITE_PETAL_POSTHOG_KEY/);
  assert.match(webAnalytics, /client: 'web'/);
  assert.match(webAnalytics, /\/i\/v0\/e\//);
  assert.doesNotMatch(webAnalytics, /phc_[A-Za-z0-9]{8,}/);
  for (const name of SHARED_EVENTS) {
    assert.match(webAnalytics, new RegExp(`'${name}'`));
  }
  for (const name of NATIVE_ONLY_EVENTS) {
    assert.doesNotMatch(webAnalytics, new RegExp(`'${name}'`));
  }
});

// #370: the latency summary is the one event allowed integer-ms values. The
// doc must scope that exception explicitly and list every contract stage.
test('allowlist documents the latency-summary exception and every stage', () => {
  const contract = JSON.parse(readFileSync(join(root, 'contracts', 'petal-contracts.json'), 'utf8'));
  const schema = contract.remoteControlLatencySummary;
  assert.match(allowlist, /\*\*Exception \(#370\): `remote_control_latency_summary`/);
  for (const key of [...schema.stages, ...Object.keys(schema.dimensions), ...schema.alwaysPresent]) {
    assert.match(allowlist, new RegExp(`\`${key}\``), `allowlist is missing ${key}`);
  }
  // The stale row (#370 cleanup): duration_bucket was removed from the event.
  assert.doesNotMatch(allowlist, /\| `remote_video_stalled` \| `duration_bucket`/);
  assert.match(analytics, /fn usage_stats_consented\(\) -> bool/);
});
