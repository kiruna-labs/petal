import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';

import {
  audioConstraintForDeviceId,
  resolvePersistedDeviceId,
  supportsAudioOutputSelection,
  videoConstraintsForDeviceId,
} from '../src/controls.ts';
import { CAMERA_VIDEO_CONSTRAINTS } from '../src/constants.ts';
import { optionsFromDevices, placeDeviceMenu, resolvePreferredDeviceId, resolveSelectedOptionId } from '../src/deviceMenu.ts';

const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const mainSource = readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8');
const controlsSource = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');
const constantsSource = readFileSync(new URL('../src/constants.ts', import.meta.url), 'utf8');
const styleSource = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
const sharedStyleSource = readFileSync(new URL('../../shared/ui/meeting-controls.css', import.meta.url), 'utf8');
const deviceMenuSource = readFileSync(new URL('../src/deviceMenu.ts', import.meta.url), 'utf8');

function device(deviceId: string): Pick<MediaDeviceInfo, 'deviceId'> {
  return { deviceId };
}

test('persisted audio device ids are used only while the device still exists', () => {
  const devices = [device('default'), device('usb-mic')];

  assert.equal(resolvePersistedDeviceId(devices, 'usb-mic'), 'usb-mic');
  assert.equal(resolvePersistedDeviceId(devices, 'missing-mic'), '');
  assert.equal(resolvePersistedDeviceId(devices, ''), '');
  assert.equal(resolvePersistedDeviceId([], 'usb-mic'), '');
});

test('microphone constraints use ideal device ids so stale persisted devices can fall back', () => {
  assert.equal(audioConstraintForDeviceId(''), true);
  assert.deepEqual(audioConstraintForDeviceId('usb-mic'), { deviceId: { ideal: 'usb-mic' } });
});

test('camera constraints merge a persisted ideal deviceId at the call site without mutating CAMERA_VIDEO_CONSTRAINTS', () => {
  assert.deepEqual(CAMERA_VIDEO_CONSTRAINTS, {
    width: { ideal: 1280 },
    height: { ideal: 720 },
    frameRate: { ideal: 30, max: 30 },
  });
  assert.deepEqual(videoConstraintsForDeviceId(''), { ...CAMERA_VIDEO_CONSTRAINTS });
  assert.deepEqual(videoConstraintsForDeviceId('facetime'), {
    ...CAMERA_VIDEO_CONSTRAINTS,
    deviceId: { ideal: 'facetime' },
  });
  assert.equal('deviceId' in CAMERA_VIDEO_CONSTRAINTS, false);
  assert.match(controlsSource, /getUserMedia\(\{ video: videoConstraintsForDeviceId\(preferredVideoId\) \}\)/);
});

test('web meeting device switching uses attached split controls', () => {
  assert.match(indexSource, /id="ctl-audio-options" class="meeting-split-options"/);
  assert.match(indexSource, /id="ctl-video-options" class="meeting-split-options"/);
  assert.match(indexSource, /aria-label="Microphone options"/);
  assert.match(indexSource, /aria-label="Camera options"/);
  assert.match(indexSource, /class="meeting-split"/);
  assert.match(indexSource, /id="ctl-draw" class="control-button"/);
  assert.doesNotMatch(indexSource, /id="audio-input-select"/);
  assert.doesNotMatch(indexSource, /id="audio-output-select"/);
  assert.match(constantsSource, /HARNESS_AUDIO_INPUT_STORAGE_KEY/);
  assert.match(constantsSource, /HARNESS_AUDIO_OUTPUT_STORAGE_KEY/);
  assert.match(constantsSource, /HARNESS_VIDEO_INPUT_STORAGE_KEY/);
  assert.match(controlsSource, /Room\.getLocalDevices\(kind,\s*false\)/);
  assert.match(controlsSource, /switchActiveDevice\(kind,\s*deviceId,\s*false\)/);
  assert.match(controlsSource, /supportsAudioOutputSelection\(\)/);
  assert.match(controlsSource, /applyVideoInputDevice/);
  assert.match(styleSource, /\.device-option > span:first-child\s*\{[\s\S]*overflow-wrap:\s*anywhere;/);
  assert.doesNotMatch(styleSource, /\.device-option > span:first-child\s*\{[\s\S]*text-overflow:\s*ellipsis/);
  assert.match(mainSource, /participantCountEl,\s*tilesEl,\s*networkDiagnosticsRows,\s*topbarRight,\s*ctlAudio/);
  assert.match(sharedStyleSource, /\.meeting-split-options\s*\{/);
  assert.match(
    sharedStyleSource,
    /\.meeting-split > \.control-button:not\(:disabled\)[\s\S]*background: transparent;/
  );
  assert.match(
    sharedStyleSource,
    /\.meeting-split > \.control-button:hover:not\(:disabled\)[\s\S]*background: var\(--fill-strong\)/
  );
  assert.match(deviceMenuSource, /device-option-check/);
  assert.match(deviceMenuSource, /ArrowDown/);
  assert.doesNotMatch(deviceMenuSource, /\.then\(\(\) => close\(\)\)/);
});

test('device menu placement prefers above the options trigger and stays on-screen', () => {
  const trigger = { top: 640, bottom: 658, left: 80, right: 108, width: 28, height: 18 } as DOMRect;
  const placed = placeDeviceMenu(trigger, { width: 240, height: 180 }, { width: 800, height: 700 });
  assert.ok(placed.top + 180 <= 640, 'menu should open above the bottom control bar');
  assert.ok(placed.left + 240 <= 800);
  assert.ok(placed.left >= 8);
});

test('listed device options skip empty ids and wrap unlabeled devices', () => {
  const options = optionsFromDevices(
    [
      { deviceId: '', label: 'ghost', kind: 'audioinput', groupId: '' } as MediaDeviceInfo,
      { deviceId: 'mic-1', label: 'Blue Yeti', kind: 'audioinput', groupId: '' } as MediaDeviceInfo,
      { deviceId: 'mic-2', label: '', kind: 'audioinput', groupId: '' } as MediaDeviceInfo,
    ],
    'Microphone'
  );
  assert.deepEqual(options, [
    { id: 'mic-1', label: 'Blue Yeti' },
    { id: 'mic-2', label: 'Microphone 2' },
  ]);
});

test('speaker output selection remains feature-gated', () => {
  assert.equal(typeof supportsAudioOutputSelection(), 'boolean');
  assert.match(controlsSource, /supportsAudioOutput: supportsAudioOutputSelection/);
});

test('the browser default pseudo-device folds onto its concrete twin', () => {
  const options = optionsFromDevices(
    [
      { deviceId: 'default', label: 'Default - USB Mic', kind: 'audioinput', groupId: 'g1' } as MediaDeviceInfo,
      { deviceId: 'usb-mic', label: 'USB Mic', kind: 'audioinput', groupId: 'g1' } as MediaDeviceInfo,
      { deviceId: 'builtin-mic', label: 'Built-in Mic', kind: 'audioinput', groupId: 'g2' } as MediaDeviceInfo,
    ],
    'Microphone'
  );
  // Exactly one row for the twinned device, relabeled and flagged -- not two.
  assert.deepEqual(options, [
    { id: 'usb-mic', label: 'USB Mic (System Default)', systemDefault: true },
    { id: 'builtin-mic', label: 'Built-in Mic' },
  ]);
});

test('a default pseudo-device with no concrete twin (unmatched groupId) is kept as its own row', () => {
  const options = optionsFromDevices(
    [
      { deviceId: 'default', label: 'Default - Headset', kind: 'audioinput', groupId: 'g9' } as MediaDeviceInfo,
      { deviceId: 'other-mic', label: 'Other Mic', kind: 'audioinput', groupId: 'g2' } as MediaDeviceInfo,
    ],
    'Microphone'
  );
  assert.deepEqual(options, [
    { id: 'default', label: 'Default - Headset' },
    { id: 'other-mic', label: 'Other Mic' },
  ]);
});

test('a default pseudo-device with no groupId at all is never folded (empty string is not a match key)', () => {
  const options = optionsFromDevices(
    [
      { deviceId: 'default', label: 'Default', kind: 'audioinput', groupId: '' } as MediaDeviceInfo,
      { deviceId: 'usb-mic', label: 'USB Mic', kind: 'audioinput', groupId: '' } as MediaDeviceInfo,
    ],
    'Microphone'
  );
  assert.deepEqual(options, [
    { id: 'default', label: 'Default' },
    { id: 'usb-mic', label: 'USB Mic' },
  ]);
});

test('picker selection prefers the active device over the persisted id, and persisted over options[0]', () => {
  const options = [{ id: 'mic-a' }, { id: 'mic-b' }, { id: 'mic-c' }] as { id: string; label: string }[];

  // Active device wins even when a different id is persisted.
  assert.equal(
    resolveSelectedOptionId(options as any, resolvePreferredDeviceId('mic-c', 'mic-b')),
    'mic-c'
  );
  // No active device -- falls back to persisted.
  assert.equal(resolveSelectedOptionId(options as any, resolvePreferredDeviceId('', 'mic-b')), 'mic-b');
  // Neither active nor persisted -- falls back to the first option, never a
  // device absent from the current list.
  assert.equal(resolveSelectedOptionId(options as any, resolvePreferredDeviceId('', '')), 'mic-a');
  // A stale active/persisted id that no longer exists in the list also falls
  // back to the first option rather than selecting nothing.
  assert.equal(
    resolveSelectedOptionId(options as any, resolvePreferredDeviceId('unplugged-mic', '')),
    'mic-a'
  );
});

test('an active or persisted id of literally "default" selects the folded twin, not options[0]', () => {
  // Same shape optionsFromDevices produces once it folds default(g1) onto B.
  // The twin is deliberately NOT options[0] here: if the 'default'->twin
  // mapping were missing, resolveSelectedOptionId's own options[0] fallback
  // would land on 'mic-A' by coincidence in a twin-first list and mask the
  // bug -- putting the twin second makes a missing mapping observably wrong.
  const optionsWithTwin = [
    { id: 'mic-A', label: 'Mic A' },
    { id: 'mic-B', label: 'Mic B', systemDefault: true },
  ];

  // Active device reported as 'default' (e.g. a track opened with
  // deviceId 'default', or room.getActiveDevice() echoing the last request).
  assert.equal(
    resolveSelectedOptionId(optionsWithTwin, resolvePreferredDeviceId('default', '')),
    'mic-B'
  );
  // Persisted 'default' from an older session, no active device this time.
  assert.equal(
    resolveSelectedOptionId(optionsWithTwin, resolvePreferredDeviceId('', 'default')),
    'mic-B'
  );
  // A real active device still wins over a stale persisted 'default'.
  assert.equal(
    resolveSelectedOptionId(optionsWithTwin, resolvePreferredDeviceId('mic-A', 'default')),
    'mic-A'
  );

  // No twin was folded (unmatched groupId) -- the 'default' row itself is
  // still a real, selectable option, so it stays selected rather than
  // falling back to options[0].
  const optionsNoTwin = [
    { id: 'default', label: 'Default - Headset' },
    { id: 'mic-B', label: 'Mic B' },
  ];
  assert.equal(
    resolveSelectedOptionId(optionsNoTwin, resolvePreferredDeviceId('default', '')),
    'default'
  );
});
