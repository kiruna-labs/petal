import assert from 'node:assert/strict'; import { test } from 'node:test'; import { cells, fixtureManifest } from '../src/fidelityFixture.ts';
import { readFileSync } from 'node:fs';
test('fixture exposes the complete 1x/2x matrix',()=>assert.deepEqual(cells.map(c=>`${c.sourceScale}-${c.receiverScale}`),['2-2','2-1','1-1','1-2']));
test('manifest forbids browser screenshot evidence and rescaling',()=>{const value=fixtureManifest(cells[0],'2026-07-11T00:00:00.000Z');assert.equal(value.capture.browserScreenshotIsValidEvidence,false);assert.equal(value.capture.method,'macOS OS-compositor screenshot');assert.equal(value.capture.rescaleBeforeScoring,false);assert.equal(value.lockedAt,'2026-07-11T00:00:00.000Z');});
test('public /fidelity route precedes invite-code rewrites',()=>{const config=JSON.parse(readFileSync(new URL('../vercel.json',import.meta.url),'utf8'));assert.deepEqual(config.rewrites[0],{source:'/fidelity',destination:'/fidelity.html'});});
