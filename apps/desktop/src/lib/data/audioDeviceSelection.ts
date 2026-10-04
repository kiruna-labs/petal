// What the mic/speaker pickers show for the saved choice, and what they say
// after a switch. Pure (no Tauri imports) so both pickers -- the in-call
// `DevicePicker.svelte` and `Settings.svelte` -- share one tested rule.
//
// The selected entry means "the saved preference", not "where audio is
// playing": a failed switch leaves the native pin as it was, and a partial
// failure can leave the device stopped, so the pickers never claim more than
// the saved choice plus the switch's own result.
import type { AppliedAudioDevices } from '../ipc.ts';

/** Picker value for a saved device id that is not in the current list. */
export const MISSING_DEVICE = 'petal:missing-device';
export const MISSING_DEVICE_LABEL = 'Saved device not connected';

export type AudioDeviceKind = 'microphone' | 'speaker';

/**
 * The entry a picker shows as selected for `savedId`.
 *
 * - Empty (no choice saved): the first entry, which is the platform default --
 *   the `''` "System default" entry on Windows, the first device on macOS.
 * - Present in the list: `savedId`.
 * - Missing from the list: `MISSING_DEVICE`, never another device, because
 *   that would name a device the saved choice is not.
 */
export function pickerValue(options: readonly { id: string }[], savedId: string): string {
  if (savedId === '') return options[0]?.id ?? '';
  return options.some((option) => option.id === savedId) ? savedId : MISSING_DEVICE;
}

/**
 * The message when capture moved off a microphone that gave only digital
 * silence (Windows: a wireless headset switched off behind its dongle). The
 * saved choice is unchanged, so the picker still checks it; this says where
 * the voice actually comes from.
 */
export function silentMicNote(silentDevice: string, deviceName: string): string {
  return `No sound from ${silentDevice} — switched to ${deviceName}`;
}

/**
 * The caption after a switch. Null only when there is no backend to ask; an
 * in-room failure always says so, with the error the backend reported.
 */
export function switchNote(
  applied: AppliedAudioDevices | null,
  kind: AudioDeviceKind
): string | null {
  if (!applied) return null;
  const switched = kind === 'microphone' ? applied.micApplied : applied.speakerApplied;
  const error = kind === 'microphone' ? applied.micError : applied.speakerError;
  if (switched) return `Switched ${kind}`;
  if (!applied.inRoom) return 'Saved — applies when you join a room';
  if (kind === 'microphone' && error === 'no live microphone track') {
    return 'Saved — microphone isn’t active in this meeting (check mic permission)';
  }
  return `Could not switch ${kind}: ${error ?? 'unknown error'}`;
}
