import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  cockpitOwnerFromSearch,
  cockpitShareTileMissingDetail,
  selectCockpitShareTile,
} from '../src/cockpitShareTarget.ts';

// #919: the DOM as DRAW-N's peer saw it on the VM -- the previous scenario's
// killed web peer still published `petal-window-116208`, and its tile came
// first; the native cockpit's window 53 was second.
const ghost = { dataset: { owner: 'web-086ea62c', windowId: '116208' } };
const native = { dataset: { owner: 'p-cockpit-1a073d49b7b1c5d', windowId: '53' } };

test('selectCockpitShareTile picks the named owner even when a lingering peer tile is first (#919)', () => {
  assert.equal(selectCockpitShareTile([ghost, native], 'p-cockpit-1a073d49b7b1c5d'), native);
  assert.equal(selectCockpitShareTile([ghost, native], ' p-cockpit-1a073d49b7b1c5d '), native);
});

test('selectCockpitShareTile never falls back to another owner when the named one is absent', () => {
  assert.equal(selectCockpitShareTile([ghost], 'p-cockpit-1a073d49b7b1c5d'), null);
  assert.equal(selectCockpitShareTile([], 'p-cockpit-1a073d49b7b1c5d'), null);
});

test('selectCockpitShareTile keeps the first-tile behaviour when no owner was named', () => {
  assert.equal(selectCockpitShareTile([ghost, native]), ghost);
  assert.equal(selectCockpitShareTile([ghost, native], '  '), ghost);
  assert.equal(selectCockpitShareTile([]), null);
});

test('cockpitOwnerFromSearch reads &owner= and ignores blanks', () => {
  assert.equal(cockpitOwnerFromSearch('?code=abc-defg-hjk&auto=draw-n&owner=p-cockpit-1a'), 'p-cockpit-1a');
  assert.equal(cockpitOwnerFromSearch('?code=abc-defg-hjk&auto=draw-n'), undefined);
  assert.equal(cockpitOwnerFromSearch('?owner=%20'), undefined);
  assert.equal(cockpitOwnerFromSearch(''), undefined);
});

test('cockpitShareTileMissingDetail names the wanted owner and what was actually present', () => {
  const detail = cockpitShareTileMissingDetail('draw', [ghost], 'p-cockpit-1a');
  assert.match(detail, /owner=p-cockpit-1a/);
  assert.match(detail, /web-086ea62c:116208/);
  assert.match(cockpitShareTileMissingDetail('telepointer', []), /requires a remote share tile/);
});
