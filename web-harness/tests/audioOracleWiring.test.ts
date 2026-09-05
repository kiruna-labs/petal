import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const controls = readFileSync(new URL('../src/controls.ts', import.meta.url), 'utf8');

test('measureCockpitRemoteAudio overlaps recording and stats, then asserts oracle agreement', () => {
  assert.match(controls, /const \[recorded, after\] = await Promise\.all\(/);
  assert.match(controls, /assertRemoteAudioOraclesAgree\(recorded\.rms, statsRms, AUDIBILITY_RMS_BAR\)/);
  assert.doesNotMatch(
    controls,
    /const recorded = await recordRemoteAudioRms\([\s\S]*const before = await readEnergy\(track\)/
  );
});
