import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const source = readFileSync(
  fileURLToPath(new URL('../src/lib/components/MeetingChrome.svelte', import.meta.url)),
  'utf8'
);

function extractArrayLiteral(name: string): string[] {
  const match = source.match(new RegExp(`const ${name} = \\[([^\\]]*)\\]`));
  assert.ok(match, `expected ${name}`);
  return Array.from(match[1].matchAll(/'([^']+)'/g), (entry) => entry[1]);
}

test('compact controls keep the essential row fixed and move secondary actions to More', () => {
  assert.deepEqual(extractArrayLiteral('DISPLAY_ORDER'), ['mic', 'camera', 'screenshare']);
  assert.deepEqual(extractArrayLiteral('PILL_MORE_ORDER'), ['invite', 'region', 'remotecontrol']);
  assert.doesNotMatch(source, /DeviceCaret/);
});

test('leave renders after More and Expand as the final pill control', () => {
  const pillOpen = source.indexOf('<Pill {orientation} scale="large">');
  const pillClose = source.indexOf('</Pill>', pillOpen);
  assert.ok(pillOpen !== -1 && pillClose !== -1);
  const body = source.slice(pillOpen, pillClose);
  const loopClose = body.indexOf('{/each}');
  const more = body.indexOf('<ControlButton\n              icon="more"');
  const expand = body.indexOf('class="pill-switcher"');
  const leave = body.indexOf('<ControlButton\n            icon="leave"');
  assert.ok(loopClose !== -1 && more !== -1 && expand !== -1 && leave !== -1);
  assert.ok(leave > loopClose);
  assert.ok(leave > more);
  assert.ok(leave > expand);
  assert.equal((body.slice(leave).match(/<ControlButton/g) ?? []).length, 1);
});

test('pill sizing budgets the attached mic/camera option segments', () => {
  assert.match(source, /const SPLIT_EXTRA = 22;/);
  assert.match(source, /Math\.min\(buttons, 2\) \* SPLIT_EXTRA/);
  assert.match(source, /Math\.min\(GUARANTEED_VISIBLE\.length, 2\) \* SPLIT_EXTRA/);
  assert.match(source, /const minCrossAxis = 66;/);
});
