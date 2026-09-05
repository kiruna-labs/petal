import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  AUDIBILITY_RMS_BAR,
  assertRemoteAudioOraclesAgree,
} from '../src/audioOracleAgreement.ts';

test('audio oracles that agree across the audibility bar do not throw', () => {
  assert.equal(AUDIBILITY_RMS_BAR, 0.01);
  assert.doesNotThrow(() => assertRemoteAudioOraclesAgree(0.35, 0.35));
  assert.doesNotThrow(() => assertRemoteAudioOraclesAgree(0, 0));
  assert.doesNotThrow(() => assertRemoteAudioOraclesAgree(0.02, 0.04));
});

test('audio oracles that straddle the audibility bar throw INFRA, not a product ok', () => {
  assert.throws(
    () => assertRemoteAudioOraclesAgree(0.35, 0),
    /disagree across the 0.01 audibility bar/
  );
  assert.throws(
    () => assertRemoteAudioOraclesAgree(0, 0.35),
    /disagree across the 0.01 audibility bar/
  );
  assert.throws(
    () => assertRemoteAudioOraclesAgree(0.009, 0.011),
    /disagree across the 0.01 audibility bar/
  );
});

test('unavailable recording does not throw; stats remain the fallback oracle', () => {
  assert.doesNotThrow(() => assertRemoteAudioOraclesAgree(-1, 0));
  assert.doesNotThrow(() => assertRemoteAudioOraclesAgree(-1, 0.35));
});
