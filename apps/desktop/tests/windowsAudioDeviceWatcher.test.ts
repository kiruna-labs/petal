import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const session = readFileSync(new URL('../src-tauri/src/session_stub.rs', import.meta.url), 'utf8');
const toastHost = readFileSync(new URL('../src/lib/components/ToastHost.svelte', import.meta.url), 'utf8');
const picker = readFileSync(new URL('../src/lib/components/DevicePicker.svelte', import.meta.url), 'utf8');
const ipc = readFileSync(new URL('../src/lib/ipc.ts', import.meta.url), 'utf8');

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

test('the open in-call picker re-lists when Windows audio devices come and go', () => {
  const watcher = watcherSource();
  // Sent last in the tick, after any restore or fallback has switched.
  const emit = watcher.indexOf('tauri::Emitter::emit(&app, "audio-devices-changed", ())');
  assert.ok(emit > watcher.indexOf('state.refresh_audio_devices()'), 'emit after the refresh');
  assert.match(watcher, /audio_endpoints_changed\(&mut last_endpoints,/);
  assert.match(ipc, /audioDevicesChanged: 'audio-devices-changed'/);

  assert.match(picker, /listenUntilDestroy\(\s*EVENTS\.audioDevicesChanged,/);
  assert.match(picker, /loadAudioLists\(\)\.catch\(/);
  // A slower, older listing never overwrites a newer one.
  assert.match(picker, /if \(request !== audioListRequest\) return;/);
  assert.match(picker, /unlistenAudioDevices\?\.\(\);/);
});
