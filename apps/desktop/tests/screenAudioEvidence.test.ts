import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Live acceptance for per-share output audio has to prove two things that are
// otherwise unfalsifiable from a log who only ever wrote FAILURE paths:
//
//   (a) a fresh share with the control untouched publishes ZERO companion
//       tracks, and
//   (b) after opt-in, stop/republish and a reconnect leave exactly ONE intended
//       companion and no stale track.
//
// "No failure line appeared" proves neither. These tests pin the bounded
// evidence lines those two claims are read from, so a later refactor cannot
// silently delete the only proof the runbook depends on.

const AUDIO_SOURCE = readFileSync(
  new URL('../src-tauri/src/transport/audio.rs', import.meta.url),
  'utf8'
);
const SCREEN_AUDIO_SOURCE = readFileSync(
  new URL('../src-tauri/src/screen_audio.rs', import.meta.url),
  'utf8'
);

const PUBLISHED = 'audio: screen-audio published scope={} track={} sid={}';
const STOPPED = 'audio: screen-audio stopped scope={} track={}';
const REPUBLISHED = 'audio: screen-audio republished after reconnect scope={} track={}';

/** The `log::<level>!` call that owns a format string. */
function logLevelFor(source: string, format: string): string | null {
  const index = source.indexOf(format);
  if (index < 0) return null;
  const callStart = source.lastIndexOf('log::', index);
  if (callStart < 0) return null;
  const match = /^log::(\w+)!/.exec(source.slice(callStart));
  return match ? match[1]! : null;
}

test('every companion evidence line exists and is info-level', () => {
  // Level matters, not just presence: `petal.log` defaults to `info`, so an
  // evidence line written at `debug` is invisible in a real run -- the same
  // trap that hides the ownership coordinator's camera-decline line.
  for (const format of [PUBLISHED, STOPPED, REPUBLISHED]) {
    assert.equal(
      logLevelFor(AUDIO_SOURCE, format),
      'info',
      `${format} must exist at info level so a default-level petal.log can prove publication counts`
    );
  }
});

test('the published line separates scope from identity', () => {
  // `scope` is a class (system|process); the pid-bearing track name is a value,
  // and the sid is what distinguishes two companions in the same scope.
  assert.match(AUDIO_SOURCE, /source\.scope_label\(\),\s*track_name,\s*track\.sid\(\)/);
  assert.match(AUDIO_SOURCE, /self\.source\.scope_label\(\),\s*self\.source\.track_name\(\)/);
});

test('the scope label is a bounded class that cannot carry a pid', () => {
  const start = SCREEN_AUDIO_SOURCE.indexOf('fn scope_label');
  assert.ok(start >= 0, 'scope_label must exist');
  const end = SCREEN_AUDIO_SOURCE.indexOf('\n    }', start);
  const scope = SCREEN_AUDIO_SOURCE.slice(start, end > start ? end : undefined);
  assert.ok(scope.length > 0, 'scope_label must exist');
  assert.match(scope, /"system"/);
  assert.match(scope, /"process"/);
  // A `format!` here would mean the pid leaks into every evidence line.
  assert.doesNotMatch(
    scope,
    /format!/,
    'scope_label must return a static class, never a formatted pid'
  );
  assert.match(scope, /Self::Process\(_\)/, 'the pid must be ignored, not bound');
});

test('publishing and stopping are paired, and stop is idempotent', () => {
  const stop = AUDIO_SOURCE.slice(
    AUDIO_SOURCE.indexOf('pub(crate) async fn stop('),
    AUDIO_SOURCE.indexOf('impl Drop for ScreenAudioTrack')
  );
  // The early return on `stopped.swap` is what keeps `published - stopped`
  // meaningful: a double stop must not log twice and make the count negative.
  assert.match(
    stop,
    /if self\.stopped\.swap\(true, Ordering::AcqRel\) \{\s*return;\s*\}/,
    'stop must log at most once per track'
  );
  assert.ok(
    stop.indexOf('swap(true') < stop.indexOf(STOPPED),
    'the idempotence guard must precede the evidence line'
  );
});
