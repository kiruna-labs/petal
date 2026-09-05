import assert from 'node:assert/strict';
import test from 'node:test';

import { cameraOffNameLabelForFit, firstGrapheme, nameChipLabelForFit } from '../src/lib/data/nameChipFit.ts';

test('name-chip label keeps the full name when the measured text fits', () => {
  assert.equal(nameChipLabelForFit('Ada Lovelace', 80, 96), 'Ada Lovelace');
});

test('name-chip label falls back to one grapheme when the full name does not fit', () => {
  assert.equal(nameChipLabelForFit('Ada Lovelace', 120, 96), 'A');
});

test('name-chip fallback uses a grapheme cluster, not a partial unicode sequence', () => {
  assert.equal(firstGrapheme(' 👩🏽‍💻 Ada '), '👩🏽‍💻');
  assert.equal(nameChipLabelForFit('👩🏽‍💻 Ada', 120, 40), '👩🏽‍💻');
});

test('name-chip fallback does not render whitespace-only labels', () => {
  assert.equal(firstGrapheme('   '), '');
  assert.equal(nameChipLabelForFit('', 0, 0), '');
});

test('camera-off centered label keeps the full name when the display text fits', () => {
  assert.equal(cameraOffNameLabelForFit('Ada Lovelace', 140, 160), 'Ada Lovelace');
});

test('camera-off centered label falls back to one grapheme when the full name does not fit', () => {
  assert.equal(cameraOffNameLabelForFit('Ada Lovelace', 180, 96), 'A');
  assert.equal(cameraOffNameLabelForFit('👩🏽‍💻 Ada', 180, 96), '👩🏽‍💻');
});

// #676 hysteresis: separate grow/shrink thresholds so a fullNameWidth/
// availableWidth pair that lands within a few px of the fit boundary can't
// flip the label every measurement.
test('name-chip label keeps showing the full name across a shrink within tolerance', () => {
  // Was showing the full name (previousLabel === name); a sub-pixel-level
  // regression (avail drops 0.3px below fullNameWidth) must not flip it --
  // preserves the original 0.5px tolerance.
  assert.equal(nameChipLabelForFit('Ada Lovelace', 96, 95.7, 'Ada Lovelace'), 'Ada Lovelace');
});

test('name-chip label still shrinks once genuinely overflowing, even mid-hysteresis', () => {
  assert.equal(nameChipLabelForFit('Ada Lovelace', 100, 90, 'Ada Lovelace'), 'A');
});

test('name-chip label does not grow back the instant it technically fits again', () => {
  // Was compact; availableWidth now exactly equals fullNameWidth (fits with
  // zero margin) -- without hysteresis this would flip straight back to the
  // full name every time available width settles right at the boundary.
  assert.equal(nameChipLabelForFit('Ada Lovelace', 90, 90, 'A'), 'A');
});

test('name-chip label grows back once there is real headroom, not just a technical fit', () => {
  assert.equal(nameChipLabelForFit('Ada Lovelace', 80, 90, 'A'), 'Ada Lovelace');
});

test('name-chip label with no previousLabel uses the conservative (grow) threshold', () => {
  // First measurement (no previous render to compare against) -- fits by
  // only 2px, less than the 4px grow headroom, so it should NOT jump
  // straight to the full name on an unknown starting state.
  assert.equal(nameChipLabelForFit('Ada Lovelace', 94, 96), 'A');
});
