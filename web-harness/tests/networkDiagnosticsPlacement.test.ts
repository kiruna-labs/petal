import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const index = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const diagnostics = readFileSync(new URL('../src/networkDiagnostics.ts', import.meta.url), 'utf8');

test('network diagnostics live inside the closed-by-default debug drawer, not above the control bar', () => {
  const networkIdx = index.indexOf('id="network-panel"');
  const rowsIdx = index.indexOf('id="network-diagnostics-rows"');
  const controlIdx = index.indexOf('class="controlbar"');
  const devIdx = index.indexOf('id="dev-panel"');

  assert.notEqual(networkIdx, -1);
  assert.notEqual(rowsIdx, -1);
  assert.notEqual(controlIdx, -1);
  assert.notEqual(devIdx, -1);
  assert.ok(controlIdx < networkIdx, 'network panel must not sit above the control bar');
  assert.ok(devIdx < networkIdx, 'network panel must be inside #dev-panel');
  assert.ok(networkIdx < rowsIdx, 'rows must stay inside #network-panel');
  assert.doesNotMatch(index, /<details[^>]*id="network-panel"[^>]*\sopen\b/);
  assert.doesNotMatch(index, /<details[^>]*id="dev-panel"[^>]*\sopen\b/);
});

test('network diagnostics render interval is gated on both details being open', () => {
  assert.match(diagnostics, /shouldRenderNetworkDiagnostics\(devPanel, networkPanel\)/);
  assert.match(diagnostics, /addEventListener\('toggle', syncTimer\)/);
  assert.match(diagnostics, /clearInterval\(timer\)/);
});
