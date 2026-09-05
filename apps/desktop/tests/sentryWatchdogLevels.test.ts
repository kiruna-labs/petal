import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

// Quality watchdogs must not `log::error!` — that opens a Sentry issue per
// sample. Crash/join-hard-fail `error!`s stay. Locked against the 2026-08-17
// Sentry-vs-PostHog split (docs/POSTHOG_EVENT_ALLOWLIST.md).

const desktop = join(dirname(fileURLToPath(import.meta.url)), '..');
const root = join(desktop, 'src-tauri', 'src');
const vendor = join(desktop, 'vendor', 'livekit', 'src');

function read(rel: string): string {
  return readFileSync(join(rel.startsWith('rtc_engine') ? vendor : root, rel), 'utf8');
}

test('video stall and display-drop watchdogs log at warn, not error', () => {
  const diagnostics = read('diagnostics.rs');
  assert.match(diagnostics, /log::warn!\(\s*"diagnostics: \{message\}/);
  assert.match(
    diagnostics,
    /log::warn!\(\s*"diagnostics: receiver display enqueue drop rate/
  );
  assert.doesNotMatch(diagnostics, /log::error!\(\s*"diagnostics:/);
});

test('remote-audio EnteredAlarm logs at warn, not error', () => {
  const audio = read('transport/audio.rs');
  assert.match(audio, /WatchdogReport::EnteredAlarm => \{/);
  assert.match(audio, /log::warn!/);
  assert.match(audio, /analytics::remote_audio_silent/);
  assert.doesNotMatch(audio, /WatchdogReport::EnteredAlarm => log::error!/);
});

test('capture restart-in-place recovery logs at warn; unexpected pump death stays error', () => {
  const share = read('session/share.rs');
  assert.match(
    share,
    /log::warn!\(\s*"session: window \{window_id\} \{message\} -- restarting capture in place \(share stays published\)"/
  );
  assert.match(
    share,
    /log::warn!\(\s*"session: window \{window_id\} \{message\}; restarting capture in place"/
  );
  // Unexpected pump exit/panic still pages Sentry.
  assert.match(
    share,
    /log::error!\(\s*"session: window \{window_id\} \{message\}; restarting capture in place"/
  );
});

test('LiveKit reconnect attempts and slow-path monitors log at warn; real failures stay error', () => {
  const engine = read('rtc_engine/mod.rs');
  const session = read('rtc_engine/rtc_session.rs');
  assert.match(engine, /log::warn!\("restarting connection\.\.\. attempt:/);
  assert.match(engine, /log::warn!\("resuming connection\.\.\. attempt:/);
  assert.match(engine, /log::error!\("restarting connection failed:/);
  assert.match(engine, /log::error!\("resuming connection failed:/);
  assert.match(session, /log::warn!\("rtc_event is taking too much time:/);
  assert.match(session, /log::warn!\("signal_event taking too much time:/);
  assert.match(session, /log::error!\("failed to handle signal:/);
  assert.match(session, /log::error!\("\{:\?\} pc state failed"/);
});
