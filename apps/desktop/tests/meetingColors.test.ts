import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  PALETTE,
  colorForIdentity,
  identityHeaderCss,
  identityColorCss,
  identityInkCss,
  resolveMeetingColors,
  rosterFromPresence,
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

// #317: the gallery/roster/pills must use the participant's CHOSEN palette
// index (presence `paletteIndex`, from `petalIdentityPaletteIndex` metadata),
// the same source the native remote-window border uses; hash is fallback only.
test('colorForIdentity prefers the chosen palette index over the identity hash', () => {
  const identity = 'ada-317';
  const hashed = colorForIdentity(identity);
  const chosenIndex = (PALETTE.indexOf(hashed) + 1) % PALETTE.length;
  assert.equal(colorForIdentity(identity, chosenIndex), PALETTE[chosenIndex]);
  assert.equal(colorForIdentity(identity, null), hashed);
  assert.equal(colorForIdentity(identity, 9), hashed, 'out-of-range index falls back to the hash');
  assert.equal(colorForIdentity(identity, 1.5), hashed, 'non-integer index falls back to the hash');
});

test('roster and meeting colors follow the presence palette index', () => {
  const identity = 'ada-317';
  const chosenIndex = (PALETTE.indexOf(colorForIdentity(identity)) + 1) % PALETTE.length;
  const presence = [
    { identity, name: 'Ada', isLocal: false, speaking: false, micMuted: true, paletteIndex: chosenIndex }
  ];
  const [entry] = rosterFromPresence(presence);
  assert.equal(entry.identity, PALETTE[chosenIndex]);
  assert.equal(entry.resolvedColor, identityColorCss(PALETTE[chosenIndex]));
  assert.equal(
    resolveMeetingColors([{ identity, paletteIndex: chosenIndex }]).get(identity),
    identityColorCss(PALETTE[chosenIndex])
  );
  const [fallback] = rosterFromPresence([{ ...presence[0], paletteIndex: null }]);
  assert.equal(fallback.identity, colorForIdentity(identity));
});
