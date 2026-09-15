import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

import { HARNESS_AUDIO_INPUT_STORAGE_KEY, HARNESS_AUDIO_OUTPUT_STORAGE_KEY } from '../src/constants.ts';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

interface FakeDevice {
  deviceId: string;
  groupId: string;
  kind: string;
  label: string;
}

// Four fake mics + two fake speakers -- realistic-length labels so the
// UI-text-never-truncates check (below) is actually exercising something.
const AUDIO_INPUTS: FakeDevice[] = [
  { deviceId: 'mic-array', groupId: 'g-array', kind: 'audioinput', label: 'Built-in Microphone Array' },
  { deviceId: 'mic-usb', groupId: 'g-usb', kind: 'audioinput', label: 'External USB Conference Microphone' },
  { deviceId: 'mic-headset', groupId: 'g-headset', kind: 'audioinput', label: 'Wireless Headset Microphone' },
  { deviceId: 'mic-line', groupId: 'g-line', kind: 'audioinput', label: 'Line-In Audio Interface Input' },
];
const AUDIO_OUTPUTS: FakeDevice[] = [
  { deviceId: 'spk-builtin', groupId: 'g-spk1', kind: 'audiooutput', label: 'Built-in Speakers' },
  { deviceId: 'spk-headset', groupId: 'g-spk2', kind: 'audiooutput', label: 'Wireless Headset Speakers' },
];

// Not the first enumerated device in either list -- proves the checked row
// tracks the persisted/active selection, not enumeration order (options[0]).
const PERSISTED_MIC_ID = 'mic-headset';
const PERSISTED_SPEAKER_ID = 'spk-headset';

// A browser 'default' pseudo-entry twinned (by groupId) with mic-headset,
// listed first the way Chrome typically enumerates it. After folding, the
// twin (mic-headset) is NOT options[0] -- mic-array is -- so a picker that
// forgot to map a persisted 'default' id onto the twin would visibly land
// on the wrong row instead of coincidentally matching.
const AUDIO_INPUTS_WITH_DEFAULT: FakeDevice[] = [
  { deviceId: 'default', groupId: 'g-headset', kind: 'audioinput', label: 'Default - Wireless Headset Microphone' },
  ...AUDIO_INPUTS,
];

interface RenderedOption {
  deviceId: string;
  checkDisplay: string;
  rowFits: boolean;
  textFits: boolean;
}

interface RenderedField {
  sectionLabel: string;
  options: RenderedOption[];
}

async function buildHarness(buildDir: string) {
  await build({
    root: webRoot,
    configFile: false,
    logLevel: 'silent',
    base: './',
    plugins: [svelte()],
    define: {
      __PETAL_BUILD_INFO__: JSON.stringify({ version: 'test', commit: 'test', buildDate: '2099-01-01' }),
      'import.meta.env.VITE_SENTRY_DSN': JSON.stringify('')
    },
    resolve: { alias: { '@petal/shared': resolve(repoRoot, 'shared') } },
    build: {
      outDir: buildDir,
      emptyOutDir: true,
      rollupOptions: { input: resolve(webRoot, 'index.html') }
    }
  });
}

// Builds the app, fakes navigator.mediaDevices.enumerateDevices with the
// given devices, persists the given ids (simulating an earlier session --
// there's no live room/track in this headless harness, so controls.ts's
// activeDeviceId() is always '' and render() falls through to storedId;
// that's the same code path an active id would combine through, so a
// persisted 'default' exercises the same mapping in resolveSelectedOptionId
// that an active 'default' would), opens the audio picker, and returns each
// section's rendered rows.
async function openAudioPickerFields(
  buildDir: string,
  opts: { audioInputs: FakeDevice[]; audioOutputs: FakeDevice[]; persistedMicId?: string; persistedSpeakerId?: string }
): Promise<RenderedField[]> {
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });
    const page = await browser.newPage({ viewport: { width: 900, height: 700 } });

    // No getUserMedia prompt needed since controls.ts lists via
    // Room.getLocalDevices(kind, false), which only calls
    // navigator.mediaDevices.enumerateDevices().
    await page.addInitScript(
      ({ audioInputs, audioOutputs }: { audioInputs: FakeDevice[]; audioOutputs: FakeDevice[] }) => {
        const fakeDevices = [...audioInputs, ...audioOutputs];
        // @ts-expect-error -- test-only stub, not a real MediaDeviceInfo
        navigator.mediaDevices.enumerateDevices = async () => fakeDevices;
        if (!('setSinkId' in HTMLMediaElement.prototype)) {
          // Chromium already has setSinkId; this only covers a headless
          // build where it's missing, so the Speaker section still renders.
          // @ts-expect-error -- test-only stub
          HTMLMediaElement.prototype.setSinkId = async () => undefined;
        }
      },
      { audioInputs: opts.audioInputs, audioOutputs: opts.audioOutputs }
    );
    await page.addInitScript(
      ({
        micKey,
        micId,
        speakerKey,
        speakerId
      }: {
        micKey: string;
        micId?: string;
        speakerKey: string;
        speakerId?: string;
      }) => {
        if (micId) localStorage.setItem(micKey, micId);
        if (speakerId) localStorage.setItem(speakerKey, speakerId);
      },
      {
        micKey: HARNESS_AUDIO_INPUT_STORAGE_KEY,
        micId: opts.persistedMicId,
        speakerKey: HARNESS_AUDIO_OUTPUT_STORAGE_KEY,
        speakerId: opts.persistedSpeakerId
      }
    );

    await page.goto(pathToFileURL(join(buildDir, 'index.html')).href, { waitUntil: 'load' });
    await page.waitForSelector('#ctl-audio-options', { state: 'attached' });
    await page.evaluate(() => {
      document.querySelector('#meeting-screen')?.classList.remove('hidden');
      document.querySelector('#join-screen')?.classList.add('hidden');
    });

    await page.locator('#ctl-audio-options').click();
    await page.waitForFunction(() => !document.querySelector('#devices-menu')?.hasAttribute('hidden'));
    await page.waitForSelector('#devices-menu-body .device-option');

    return await page.evaluate(() => {
      return Array.from(document.querySelectorAll<HTMLElement>('#devices-menu-body .device-field')).map((field) => {
        const sectionLabel = field.querySelector('.device-field-label')?.textContent ?? '';
        const options = Array.from(field.querySelectorAll<HTMLButtonElement>('.device-option')).map((option) => {
          const check = option.querySelector<HTMLElement>('.device-option-check');
          const textSpan = option.querySelector<HTMLElement>('span:first-child');
          return {
            deviceId: option.dataset.deviceId ?? '',
            checkDisplay: check ? getComputedStyle(check).display : 'none',
            rowFits: option.scrollWidth <= option.clientWidth,
            textFits: textSpan ? textSpan.scrollWidth <= textSpan.clientWidth : true,
          };
        });
        return { sectionLabel, options };
      });
    });
  } finally {
    await browser?.close();
  }
}

test('web meeting device picker checks exactly the persisted device in each section, never every row', { timeout: 60_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-device-checkmark-build-'));
  try {
    await buildHarness(buildDir);
    const fields = await openAudioPickerFields(buildDir, {
      audioInputs: AUDIO_INPUTS,
      audioOutputs: AUDIO_OUTPUTS,
      persistedMicId: PERSISTED_MIC_ID,
      persistedSpeakerId: PERSISTED_SPEAKER_ID
    });

    assert.equal(fields.length, 2, 'expected a Microphone section and a Speaker section');
    const micSection = fields.find((f: RenderedField) => f.sectionLabel === 'Microphone');
    const speakerSection = fields.find((f: RenderedField) => f.sectionLabel === 'Speaker');
    assert.ok(micSection, 'Microphone section not found');
    assert.ok(speakerSection, 'Speaker section not found');

    for (const section of [micSection!, speakerSection!]) {
      const visibleChecks = section.options.filter((o: RenderedOption) => o.checkDisplay !== 'none');
      assert.equal(
        visibleChecks.length,
        1,
        `expected exactly one visible checkmark in "${section.sectionLabel}", got ${visibleChecks.length} ` +
          `(${JSON.stringify(section.options)})`
      );
      for (const option of section.options) {
        assert.equal(option.rowFits, true, `option row for ${option.deviceId} overflows its container`);
        assert.equal(option.textFits, true, `option label for ${option.deviceId} is clipped`);
      }
    }

    assert.equal(micSection!.options.length, AUDIO_INPUTS.length);
    assert.equal(speakerSection!.options.length, AUDIO_OUTPUTS.length);

    const checkedMic = micSection!.options.find((o: RenderedOption) => o.checkDisplay !== 'none');
    const checkedSpeaker = speakerSection!.options.find((o: RenderedOption) => o.checkDisplay !== 'none');
    // Persisted ids are deliberately not the first enumerated device --
    // this would fail if the picker fell back to options[0].
    assert.equal(checkedMic?.deviceId, PERSISTED_MIC_ID);
    assert.notEqual(checkedMic?.deviceId, AUDIO_INPUTS[0].deviceId);
    assert.equal(checkedSpeaker?.deviceId, PERSISTED_SPEAKER_ID);
    assert.notEqual(checkedSpeaker?.deviceId, AUDIO_OUTPUTS[0].deviceId);
  } finally {
    await rm(buildDir, { recursive: true, force: true });
  }
});

test('web meeting device picker maps a persisted "default" id onto its folded twin, not options[0]', { timeout: 60_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-device-checkmark-default-build-'));
  try {
    await buildHarness(buildDir);
    const fields = await openAudioPickerFields(buildDir, {
      audioInputs: AUDIO_INPUTS_WITH_DEFAULT,
      audioOutputs: [AUDIO_OUTPUTS[0]],
      // Simulates an older session that persisted the literal id 'default'
      // before this file folded the duplicate row away.
      persistedMicId: 'default'
    });

    const micSection = fields.find((f: RenderedField) => f.sectionLabel === 'Microphone');
    assert.ok(micSection, 'Microphone section not found');

    // The 'default' pseudo-device row itself must not survive the fold
    // (it has a concrete twin), and there must be exactly one option per
    // physical device.
    assert.equal(micSection!.options.length, AUDIO_INPUTS.length);
    assert.ok(!micSection!.options.some((o) => o.deviceId === 'default'), '"default" row should have been folded');

    const visibleChecks = micSection!.options.filter((o: RenderedOption) => o.checkDisplay !== 'none');
    assert.equal(visibleChecks.length, 1, `expected exactly one visible checkmark, got ${JSON.stringify(micSection!.options)}`);
    for (const option of micSection!.options) {
      assert.equal(option.rowFits, true, `option row for ${option.deviceId} overflows its container`);
      assert.equal(option.textFits, true, `option label for ${option.deviceId} is clipped`);
    }

    const checkedMic = visibleChecks[0];
    // mic-array is options[0] after the fold -- the mapped selection must
    // land on the folded twin (mic-headset) instead, or this is exactly the
    // enumeration-order guess this branch removed.
    assert.equal(checkedMic.deviceId, 'mic-headset');
    assert.notEqual(checkedMic.deviceId, 'mic-array');
  } finally {
    await rm(buildDir, { recursive: true, force: true });
  }
});
