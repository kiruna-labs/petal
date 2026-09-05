#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';

export function parseEvidenceLine(text, prefix) {
  const line = text.split(/\r?\n/).find((candidate) => candidate.startsWith(prefix));
  if (!line) throw new Error(`missing ${prefix} evidence`);
  const value = JSON.parse(line.slice(prefix.length));
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('evidence must be an object');
  return value;
}

export function validateDisplayEvidence(value, expectedScale) {
  const positiveInteger = (candidate) => Number.isSafeInteger(candidate) && candidate > 0;
  if (value.ok !== true || !positiveInteger(value.logicalWidth) || !positiveInteger(value.logicalHeight) ||
      !positiveInteger(value.pixelWidth) || !positiveInteger(value.pixelHeight)) {
    throw new Error('display evidence is incomplete');
  }
  const derivedX = value.pixelWidth / value.logicalWidth;
  const derivedY = value.pixelHeight / value.logicalHeight;
  const scales = [value.scaleX, value.scaleY, value.backingScaleFactor];
  if (scales.some((scale) => !Number.isFinite(scale) || Math.abs(scale - expectedScale) > 0.01) ||
      Math.abs(derivedX - expectedScale) > 0.01 || Math.abs(derivedY - expectedScale) > 0.01) {
    throw new Error(`display scale consensus failed for expected ${expectedScale}x`);
  }
  return { logical: `${value.logicalWidth}x${value.logicalHeight}`, pixel: `${value.pixelWidth}x${value.pixelHeight}`, scale: expectedScale };
}

export function validateSenderCapture(logText, expectedScale, referenceWidth, referenceHeight) {
  const matches = [...logText.matchAll(/configured\s+(\d+)x(\d+)px.*scale\s+([0-9.]+)/g)];
  if (!matches.length) throw new Error('sender log has no ScreenCaptureKit configured line');
  const [, width, height, scale] = matches.at(-1);
  if (Math.abs(Number(scale) - expectedScale) > 0.01) throw new Error('sender capture scale does not match display');
  if (Number(width) < referenceWidth || Number(height) < referenceHeight) throw new Error('sender capture is smaller than fixture reference');
  return { width: Number(width), height: Number(height), scale: Number(scale) };
}

function main() {
  const args = process.argv.slice(2);
  const valueAfter = (flag) => {
    const index = args.indexOf(flag);
    return index >= 0 ? args[index + 1] : undefined;
  };
  const helper = valueAfter('--helper');
  const expectedScale = Number(valueAfter('--expected-scale'));
  const senderLog = valueAfter('--sender-log');
  if (!helper || ![1, 2].includes(expectedScale)) {
    throw new Error('usage: window-fidelity-preflight.mjs --helper <fixture-binary> --expected-scale <1|2> [--sender-log <path>]');
  }
  const output = execFileSync(helper, ['--preflight-only'], { encoding: 'utf8' });
  const display = validateDisplayEvidence(parseEvidenceLine(output, 'PETAL_FIDELITY_DISPLAY='), expectedScale);
  const result = { ok: true, display, displayMutationAttempted: false };
  if (senderLog) result.senderCapture = validateSenderCapture(fs.readFileSync(senderLog, 'utf8'), expectedScale, 960 * expectedScale, 600 * expectedScale);
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try { main(); } catch (error) {
    process.stderr.write(`INFRA-FAIL: ${error.message}\n`);
    process.exitCode = 1;
  }
}
