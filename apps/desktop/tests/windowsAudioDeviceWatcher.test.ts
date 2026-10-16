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

function commandSource(name: string): string {
  const start = session.indexOf(`pub async fn ${name}(`);
  assert.ok(start >= 0, `missing ${name}`);
  const end = session.indexOf('\n}\n', start);
  assert.ok(end > start);
  return session.slice(start, end);
}

test('a silent Windows mic only asks: the watcher never switches it', () => {
  const watcher = watcherSource();
  // Decided from this tick's signal, after the probe has observed it, and
  // never from silence an endpoint muted in Windows produces.
  const observe = watcher.indexOf('mic_signal.observe(');
  const dead = watcher.indexOf('dead_mic.observe(&device_id, mic_signal.last_signal(), endpoint_muted)');
  const offer = watcher.indexOf('microphone.silent_device_fallback(&device_id)');
  assert.ok(observe >= 0 && dead > observe && offer > dead);
  assert.match(watcher, /recording_endpoint_muted\(&device_id\)/);
  assert.match(watcher, /dead_mic\.asked\(&device_id\);/);
  assert.match(
    watcher,
    /ResilienceEvent::MicSilentOffer \{\s*silent_device,\s*silent_device_id: device_id\.clone\(\),\s*device_name,\s*device_id: target_id,/
  );
  assert.doesNotMatch(watcher.slice(dead), /set_recording_device\(|switch_from_silent_device\(/);
  assert.doesNotMatch(watcher, /MicSwitchedFromSilent/);

  assert.match(
    ipc,
    /kind: 'micSilentOffer';\s*silentDevice: string;\s*silentDeviceId: string;\s*deviceName: string;\s*deviceId: string;/
  );
  // A question that stays up until answered or closed, with one action.
  assert.match(
    toastHost,
    /case 'micSilentOffer':[\s\S]*?silentMicOffer\(event\.silentDevice, event\.deviceName\),\s*autoDismiss: false,\s*dismissible: true,\s*action: \{ label: SILENT_MIC_OFFER_ACTION, run: \(\) => void acceptSilentMicOffer\(event\) \}/
  );
  assert.match(toastHost, /\{actionLabel\}\s*onAction=\{action\}/);
});

test('the switch off a silent mic happens only when the user accepts, and both the toast and the picker say so', () => {
  assert.match(
    toastHost,
    /async function acceptSilentMicOffer[\s\S]*?await switchFromSilentMic\(offer\.silentDeviceId, offer\.deviceId\);/
  );
  assert.match(ipc, /switchFromSilentMic: 'switch_from_silent_mic'/);
  assert.match(ipc, /\[COMMANDS\.switchFromSilentMic\]: \{ silentDeviceId: string; deviceId: string \};/);

  const command = commandSource('switch_from_silent_mic');
  assert.match(command, /state\.audio_device_lock\.lock\(\)\.await/);
  const switched = command.indexOf('microphone.switch_from_silent_device(&silent_device_id, &device_id)');
  assert.ok(switched >= 0);
  assert.match(command.slice(switched), /ResilienceEvent::MicSwitchedFromSilent \{\s*silent_device,\s*device_name: device_name\.clone\(\),/);
  // The saved choice is kept: the switch never touches the preferences.
  assert.doesNotMatch(command, /preferences/);
  assert.match(
    readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8'),
    /#\[cfg\(target_os = "windows"\)\]\s*session::switch_from_silent_mic,/
  );

  assert.match(ipc, /kind: 'micSwitchedFromSilent'; silentDevice: string; deviceName: string/);
  assert.match(
    toastHost,
    /case 'micSwitchedFromSilent':[\s\S]*?silentMicNote\(event\.silentDevice, event\.deviceName\)/
  );
  assert.match(picker, /payload\.kind === 'micSwitchedFromSilent'/);
  assert.match(picker, /micNote = silentMicNote\(payload\.silentDevice, payload\.deviceName\);/);
  assert.match(picker, /unlistenResilience\?\.\(\);/);
});
