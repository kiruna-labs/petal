import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const session = readFileSync(new URL('../src-tauri/src/session_stub.rs', import.meta.url), 'utf8');
const toastHost = readFileSync(new URL('../src/lib/components/ToastHost.svelte', import.meta.url), 'utf8');

function watcherSource(): string {
  const start = session.indexOf('fn start_audio_device_watcher(');
  assert.ok(start >= 0, 'missing Windows audio-device watcher');
  const end = session.indexOf('\n}\n', start);
  assert.ok(end > start);
  return session.slice(start, end);
}

test('the Windows watcher restores a reconnected saved device before following the default', () => {
  const watcher = watcherSource();
  const restore = watcher.indexOf('state.restore_saved_audio_devices(');
  const refresh = watcher.indexOf('state.refresh_audio_devices()');
  assert.ok(restore >= 0 && refresh > restore, 'restore must run first in each tick');
});

test('the Windows watcher keeps the saved device when it falls back mid-call', () => {
  const watcher = watcherSource();
  assert.doesNotMatch(watcher, /\.set_recording_device\(String::new\(\)\)/);
  assert.doesNotMatch(watcher, /\.set_playout_device\(String::new\(\)\)/);
  // `usingDefault: true` is what makes the frontend reset its saved choice.
  assert.doesNotMatch(watcher, /using_default: Some\(true\)/);
  assert.match(toastHost, /if \(event\.usingDefault\) updateAudioDevices\('', undefined\);/);
  assert.match(toastHost, /if \(event\.usingDefault\) updateAudioDevices\(undefined, ''\);/);
});
