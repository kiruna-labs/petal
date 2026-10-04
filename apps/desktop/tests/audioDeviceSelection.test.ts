import assert from 'node:assert/strict';
import test from 'node:test';
import {
  MISSING_DEVICE,
  MISSING_DEVICE_LABEL,
  pickerValue,
  switchNote
} from '../src/lib/data/audioDeviceSelection.ts';
import type { AppliedAudioDevices } from '../src/lib/ipc.ts';

// `list_audio_devices` prepends an empty-id "System default" on Windows only.
const windowsSpeakers = [
  { id: '', label: 'System default' },
  { id: '{0.0.0.00000000}.{speakers}', label: 'Speakers' },
  { id: '{0.0.0.00000000}.{headset}', label: 'Headset' }
];
const macSpeakers = [
  { id: 'BuiltInSpeakerDevice', label: 'MacBook Pro Speakers' },
  { id: 'headset-uid', label: 'Headset' }
];

function applied(overrides: Partial<AppliedAudioDevices>): AppliedAudioDevices {
  return {
    micApplied: false,
    speakerApplied: false,
    inRoom: true,
    micError: null,
    speakerError: null,
    ...overrides
  };
}

test('nothing saved shows the platform default the device actually uses', () => {
  assert.equal(pickerValue(windowsSpeakers, ''), '', 'Windows: the System default entry');
  assert.equal(pickerValue(macSpeakers, ''), 'BuiltInSpeakerDevice', 'macOS: the first device');
  assert.equal(pickerValue([], ''), '');
});

test('a saved device that is listed is shown as selected', () => {
  assert.equal(
    pickerValue(windowsSpeakers, '{0.0.0.00000000}.{headset}'),
    '{0.0.0.00000000}.{headset}'
  );
  assert.equal(pickerValue(macSpeakers, 'headset-uid'), 'headset-uid');
});

test('a saved device that is not listed is reported, never swapped for another device', () => {
  for (const options of [windowsSpeakers, macSpeakers, []]) {
    assert.equal(pickerValue(options, 'gone-headset'), MISSING_DEVICE);
  }
  assert.equal(MISSING_DEVICE_LABEL, 'Saved device not connected');
});

test('the Windows System default id and the missing sentinel cannot collide', () => {
  assert.notEqual(MISSING_DEVICE, '');
  assert.equal(pickerValue(windowsSpeakers, ''), '');
  assert.notEqual(pickerValue(windowsSpeakers, 'gone-headset'), '');
  for (const options of [windowsSpeakers, macSpeakers]) {
    assert.ok(!options.some((option) => option.id === MISSING_DEVICE));
  }
});

test('a switch that took says so', () => {
  assert.equal(switchNote(applied({ micApplied: true }), 'microphone'), 'Switched microphone');
  assert.equal(switchNote(applied({ speakerApplied: true }), 'speaker'), 'Switched speaker');
});

test('outside a room the choice is saved for the next join', () => {
  for (const kind of ['microphone', 'speaker'] as const) {
    assert.equal(
      switchNote(applied({ inRoom: false }), kind),
      'Saved — applies when you join a room'
    );
  }
});

test('an in-room failure is never silent and carries the backend error', () => {
  assert.equal(
    switchNote(
      applied({ speakerError: 'failed to switch playout device: init_playout failed' }),
      'speaker'
    ),
    'Could not switch speaker: failed to switch playout device: init_playout failed'
  );
  assert.equal(
    switchNote(applied({ micError: 'failed to switch recording device' }), 'microphone'),
    'Could not switch microphone: failed to switch recording device'
  );
  // The other side's error is not this side's.
  assert.equal(
    switchNote(applied({ micError: 'mic broke' }), 'speaker'),
    'Could not switch speaker: unknown error'
  );
  assert.equal(
    switchNote(applied({ micError: 'no live microphone track' }), 'microphone'),
    'Saved — microphone isn’t active in this meeting (check mic permission)'
  );
});

test('no backend means no note', () => {
  assert.equal(switchNote(null, 'speaker'), null);
});
