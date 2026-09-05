// Real mic/speaker device client (issue #28) — wraps the Tauri commands
// in src-tauri/src/transport/audio.rs (`list_audio_devices`/
// `set_audio_devices`, riding the existing livekit `PlatformAudio` surface).
//
// Graceful degradation: a plain browser preview (no `__TAURI_INTERNALS__`
// bridge) has no backend — `listAudioDevices` returns null so callers keep
// their static sample options, same pattern as `permissions.ts`/`rooms.ts`.
//
// Persistence split (see the Rust module's own doc comment): the DURABLE
// store for the user's choice is the frontend session store
// (`session.svelte.ts`, localStorage); the Rust side holds an in-memory
// mirror read at join time. `seedAudioDevicePreferences()` (called from the
// root layout on startup) pushes the persisted choice into that mirror so
// "apply on next join" survives an app restart.
import { invoke } from '@tauri-apps/api/core';
import { session } from '$lib/stores/session.svelte';
import { COMMANDS, hasTauriBridge } from '$lib/ipc';
import type { AppliedAudioDevices, AudioDeviceInfo, AudioDeviceLists } from '$lib/ipc';

export { hasTauriBridge } from '$lib/ipc';
export type { AppliedAudioDevices, AudioDeviceInfo, AudioDeviceLists } from '$lib/ipc';

/**
 * Enumerate the machine's real recording/playout devices. Returns null when
 * there's no Tauri backend (plain browser); throws (with the backend's
 * honest error string) when enumeration itself fails, so callers can show
 * "audio devices unavailable" rather than silently keeping sample options.
 */
export async function listAudioDevices(): Promise<AudioDeviceLists | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<AudioDeviceLists>(COMMANDS.listAudioDevices);
}

/**
 * Record + apply a device selection. Preference is always recorded (applied
 * on the next join); additionally hot-swaps live when in a room. Returns
 * null when there's no Tauri backend.
 */
export async function setAudioDevices(opts: {
  recordingId?: string;
  playoutId?: string;
}): Promise<AppliedAudioDevices | null> {
  if (!hasTauriBridge()) return null;
  return await invoke<AppliedAudioDevices>(COMMANDS.setAudioDevices, {
    recordingId: opts.recordingId ?? null,
    playoutId: opts.playoutId ?? null
  });
}

/**
 * Push the persisted (localStorage) device preference into the Rust side's
 * in-memory mirror on startup, so a join that happens before Settings is
 * ever opened still publishes from the chosen devices. No-op without a
 * bridge or without a persisted choice.
 */
export async function seedAudioDevicePreferences(): Promise<void> {
  if (!hasTauriBridge()) return;
  const recordingId = session.micDeviceId || undefined;
  const playoutId = session.speakerDeviceId || undefined;
  if (!recordingId && !playoutId) return;
  try {
    await setAudioDevices({ recordingId, playoutId });
  } catch (e) {
    console.warn('seedAudioDevicePreferences failed', e);
  }
}
