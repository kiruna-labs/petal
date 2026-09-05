import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  PALETTE,
  identityHeaderCss,
  identityColorCss,
  identityInkCss,
  resolveMeetingColors,
  type MeetingColorParticipant
} from '../src/lib/data/rooms.ts';

function entriesFor(participants: MeetingColorParticipant[]): [string, string][] {
  return Array.from(resolveMeetingColors(participants).entries()).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0
  );
}

test('resolveMeetingColors is deterministic regardless of roster order', () => {
  const roster: MeetingColorParticipant[] = [
    { identity: 'zoe', baseColor: 'blue' },
    { identity: 'ada', baseColor: 'blue' }
  ];

  assert.deepEqual(entriesFor(roster), entriesFor([...roster].reverse()));
});

test('resolveMeetingColors tints a two-way base-color collision', () => {
  const colors = resolveMeetingColors([
    { identity: 'zoe', baseColor: 'blue' },
    { identity: 'ada', baseColor: 'blue' }
  ]);

  assert.equal(colors.get('ada'), identityColorCss('blue'));
  assert.notEqual(colors.get('zoe'), identityColorCss('blue'));
  assert.notEqual(colors.get('ada'), colors.get('zoe'));
});

test('resolveMeetingColors creates distinct colors for a three-way collision', () => {
  const colors = resolveMeetingColors([
    { identity: 'marco', baseColor: 'amber' },
    { identity: 'ada', baseColor: 'amber' },
    { identity: 'zoe', baseColor: 'amber' }
  ]);

  const resolved = ['ada', 'marco', 'zoe'].map((identity) => colors.get(identity));
  assert.equal(resolved[0], identityColorCss('amber'));
  assert.equal(new Set(resolved).size, 3);
});

test('resolveMeetingColors leaves non-colliding base colors stable', () => {
  const colors = resolveMeetingColors([
    { identity: 'plum-user', baseColor: 'plum' },
    { identity: 'blue-user', baseColor: 'blue' },
    { identity: 'green-user', baseColor: 'green' }
  ]);

  assert.equal(colors.get('plum-user'), identityColorCss('plum'));
  assert.equal(colors.get('blue-user'), identityColorCss('blue'));
  assert.equal(colors.get('green-user'), identityColorCss('green'));
});

test('identityHeaderCss defines ink pairing for every palette color', () => {
  for (const color of PALETTE) {
    const headerCss = identityHeaderCss(color);
    assert.match(headerCss, new RegExp(identityColorCss(color).replace('#', '#?'), 'i'));
    assert.match(headerCss, new RegExp(identityInkCss(color).replace('#', '#?'), 'i'));
  }
});
