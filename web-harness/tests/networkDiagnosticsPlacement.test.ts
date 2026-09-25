import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const index = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const style = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
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

// #239: the drawer keeps its DOM place (automation clicks #share-btn, and
// the order above). On desktop it stays the meeting column's bottom row, as
// testers use it; on a phone it is a sheet parked below the screen, which
// `.is-open` (?dev=1, openDevTools) slides up.
test('the developer drawer is the desktop bottom row and a parked sheet on phones', () => {
  const devPanel = /\n\.dev-panel\s*\{(?<body>[^}]+)\}/.exec(style)?.groups?.body ?? '';
  assert.match(devPanel, /flex-shrink\s*:\s*0/);
  assert.doesNotMatch(devPanel, /position\s*:/, 'in flow on desktop');

  const phone =
    /@media \(orientation: landscape\) and \(max-height: 500px\) and \(min-aspect-ratio: 3\/2\) and \(pointer: coarse\),\s*\(pointer: coarse\) and \(max-width: 560px\) \{(?<body>[\s\S]+?)\n\}/.exec(
      style
    )?.groups?.body ?? '';
  const sheet = /\.dev-panel\s*\{(?<body>[^}]+)\}/.exec(phone)?.groups?.body ?? '';
  assert.match(sheet, /position\s*:\s*fixed/);
  assert.match(sheet, /translate\s*:\s*0 100%/, 'parked below the screen');
  assert.match(sheet, /max-height\s*:\s*45dvh/);
  assert.match(phone, /\.dev-panel\.is-open\s*\{[^}]*translate\s*:\s*0 0/);

  // The meeting itself is pinned and the page under it locked: nothing can
  // scroll the drawer (or anything else) into view by accident.
  const meeting = /\n\.meeting\s*\{(?<body>[^}]+)\}/.exec(style)?.groups?.body ?? '';
  assert.match(meeting, /position\s*:\s*fixed/);
  assert.match(meeting, /inset\s*:\s*0/);
  // (The rule's selector list also takes in body: #244's no-pull-to-refresh.)
  const lock = /html:has\(> body > #meeting-screen:not\(\.hidden\)\)[^{]*\{(?<body>[^}]*)\}/.exec(style)?.groups?.body ?? '';
  assert.match(lock, /overflow\s*:\s*hidden/);
  // Pull-to-refresh must not reload the page mid-call.
  assert.match(lock, /overscroll-behavior\s*:\s*none/);
});
