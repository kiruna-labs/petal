import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseEvidenceLine, validateDisplayEvidence, validateSenderCapture } from './window-fidelity-preflight.mjs';

test('parses prefixed display JSON without accepting unrelated lines', () => {
  const evidence = parseEvidenceLine('noise\nPETAL_FIDELITY_DISPLAY={"ok":true,"logicalWidth":960}\n', 'PETAL_FIDELITY_DISPLAY=');
  assert.equal(evidence.logicalWidth, 960);
  assert.throws(() => parseEvidenceLine('noise', 'PETAL_FIDELITY_DISPLAY='));
});

test('requires five-way display scale consensus', () => {
  const retina = { ok: true, logicalWidth: 1512, logicalHeight: 982, pixelWidth: 3024, pixelHeight: 1964, scaleX: 2, scaleY: 2, backingScaleFactor: 2 };
  assert.deepEqual(validateDisplayEvidence(retina, 2), { logical: '1512x982', pixel: '3024x1964', scale: 2 });
  assert.throws(() => validateDisplayEvidence({ ...retina, backingScaleFactor: 1 }, 2), /consensus/);
  assert.throws(() => validateDisplayEvidence({ ...retina, pixelWidth: 0 }, 2), /incomplete/);
});

test('sender capture parser selects the last configuration and enforces scale and minimum reference', () => {
  const log = 'configured 960x632px at 30fps via DirectWindowId (resolution Auto, backing 960x632px, scale 1.00)\nconfigured 1920x1264px at 30fps via DirectWindowId (resolution Auto, backing 1920x1264px, scale 2.00)';
  assert.deepEqual(validateSenderCapture(log, 2, 1920, 1200), { width: 1920, height: 1264, scale: 2 });
  assert.throws(() => validateSenderCapture(log, 1, 960, 600), /scale/);
  assert.throws(() => validateSenderCapture('no capture', 2, 1920, 1200), /no ScreenCaptureKit/);
});
