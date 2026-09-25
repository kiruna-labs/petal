import { readFileSync } from 'node:fs';
import test from 'node:test';
import assert from 'node:assert/strict';

import {
  COLLAPSE_ORDER,
  barLength,
  collapseCandidates,
  placeOverflowMenu,
  planOverflow,
  type BarLine,
} from '../src/controlOverflow.ts';

const indexSource = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const pluginsSource = readFileSync(new URL('../src/plugins/setupPlugins.ts', import.meta.url), 'utf8');

// The browser bar's shape at phone width (style.css): 12px padding, 8px gaps,
// `.controls-left` nested in `.controlbar`, split Mic/Camera 70px wide, every
// other cell 44px. Its natural length is 484px, wider than any portrait phone.
function phoneBar(): BarLine<string> {
  return {
    gap: 8,
    inset: 24,
    items: [
      {
        gap: 8,
        inset: 0,
        items: [
          { cell: 'mic', size: 70 },
          { cell: 'camera', size: 70 },
          { cell: 'share', size: 44 },
          { cell: 'invite', size: 44 },
          { cell: 'draw', size: 44 },
          { cell: 'chat', size: 44 },
          { cell: 'react', size: 44 },
          { cell: 'more', size: 44 },
        ],
      },
      { cell: 'leave', size: 44 },
    ],
  };
}
const PHONE_CANDIDATES = ['react', 'draw', 'invite', 'chat', 'share'];

test('barLength counts shown cells, the gaps between them, and each line inset', () => {
  const line = phoneBar();
  assert.equal(barLength(line, (cell) => cell !== 'more'), 484);
  assert.equal(barLength(line, () => true), 484 + 44 + 8);
  // A nested line with nothing shown is still a flex item of its parent.
  assert.equal(barLength(line, (cell) => cell === 'leave'), 24 + 0 + 8 + 44);
});

test('planOverflow collapses nothing, and shows no ⋯, when the bar fits', () => {
  assert.deepEqual([...planOverflow(phoneBar(), 484, PHONE_CANDIDATES, 'more')], []);
  assert.deepEqual([...planOverflow(phoneBar(), 1280, PHONE_CANDIDATES, 'more')], []);
});

test('planOverflow collapses lowest priority first, and only as far as needed once ⋯ takes its room', () => {
  // 412: React and Draw would free enough on their own, but ⋯ costs a cell.
  assert.deepEqual([...planOverflow(phoneBar(), 412, PHONE_CANDIDATES, 'more')], ['react', 'draw', 'invite']);
  assert.deepEqual([...planOverflow(phoneBar(), 360, PHONE_CANDIDATES, 'more')], ['react', 'draw', 'invite', 'chat']);
  assert.deepEqual([...planOverflow(phoneBar(), 320, PHONE_CANDIDATES, 'more')], PHONE_CANDIDATES);
  // One pixel short collapses one cell -- and then one more to pay for ⋯.
  assert.deepEqual([...planOverflow(phoneBar(), 483, PHONE_CANDIDATES, 'more')], ['react', 'draw']);
  // Too narrow for even Mic, Camera, ⋯ and Leave: everything that can go, goes.
  assert.deepEqual([...planOverflow(phoneBar(), 200, PHONE_CANDIDATES, 'more')], PHONE_CANDIDATES);
});

test('planOverflow keeps room for a ⋯ that shows anyway (the menu has items of its own)', () => {
  assert.deepEqual([...planOverflow(phoneBar(), 536, PHONE_CANDIDATES, 'more', true)], []);
  // 484 fits every control, but not with ⋯ beside them.
  assert.deepEqual([...planOverflow(phoneBar(), 484, PHONE_CANDIDATES, 'more', true)], ['react']);
});

test('collapse order: plugins and unknown cells first (furthest along first), then Draw, Invite, Full screen, Chat, Share; never Mic, Camera or Leave', () => {
  assert.deepEqual(COLLAPSE_ORDER, ['ctl-draw', 'ctl-invite', 'ctl-fullscreen', 'ctl-chat', 'ctl-share']);
  const bar = [
    { cell: 'mic', controlId: 'ctl-audio' },
    { cell: 'camera', controlId: 'ctl-video' },
    { cell: 'share', controlId: 'ctl-share' },
    { cell: 'invite', controlId: 'ctl-invite' },
    { cell: 'draw', controlId: 'ctl-draw' },
    { cell: 'chat', controlId: 'ctl-chat' },
    { cell: 'react', controlId: '' },
    { cell: 'poll', controlId: '' },
    { cell: 'fullscreen', controlId: 'ctl-fullscreen' },
    { cell: 'leave', controlId: 'ctl-leave' },
  ];
  // Full screen (#239's rail) outlasts Draw and Invite.
  assert.deepEqual(collapseCandidates(bar), ['poll', 'react', 'draw', 'invite', 'fullscreen', 'chat', 'share']);
});

test('the menu opens above a bottom bar, right-aligned with ⋯, and inside the viewport', () => {
  const viewport = { width: 360, height: 780 };
  const bar = { left: 0, top: 700, right: 360, bottom: 780 };
  const more = { left: 250, top: 712, right: 294, bottom: 756 };
  assert.deepEqual(placeOverflowMenu(more, bar, { width: 200, height: 180 }, false, viewport), { left: 94, top: 512 });
  // A menu wider than the room left of ⋯ is clamped to the 8px margin.
  assert.deepEqual(placeOverflowMenu(more, bar, { width: 300, height: 180 }, false, viewport), { left: 8, top: 512 });
  // Taller than the room above: pinned to the top margin (the menu scrolls).
  assert.deepEqual(placeOverflowMenu(more, bar, { width: 200, height: 900 }, false, viewport), { left: 94, top: 8 });
  // Whole pixels, whatever the layout's fractions.
  const fractional = { left: 250.4, top: 712.6, right: 294.4, bottom: 756.6 };
  assert.deepEqual(placeOverflowMenu(fractional, { ...bar, top: 700.3 }, { width: 200.2, height: 180.5 }, false, viewport), { left: 94, top: 512 });
});

test('beside a vertical rail the menu opens on the free side, level with ⋯', () => {
  const viewport = { width: 844, height: 390 };
  const rail = { left: 790, top: 0, right: 844, bottom: 390 };
  const more = { left: 795, top: 250, right: 839, bottom: 294 };
  assert.deepEqual(placeOverflowMenu(more, rail, { width: 200, height: 100 }, true, viewport), { left: 582, top: 250 });
  // Taller than the room below ⋯: moved up to stay on screen.
  assert.deepEqual(placeOverflowMenu(more, rail, { width: 200, height: 200 }, true, viewport), { left: 582, top: 182 });
  // A rail on the LEFT edge: the menu flips to its right.
  const leftRail = { left: 0, top: 0, right: 54, bottom: 390 };
  assert.deepEqual(placeOverflowMenu({ ...more, left: 5, right: 49 }, leftRail, { width: 200, height: 100 }, true, viewport), {
    left: 62,
    top: 250,
  });
});

test('⋯ is a menu button that starts hidden, and plugin cells land before it', () => {
  assert.match(
    indexSource,
    /<div class="control-cell overflow-cell" hidden>\s*<button id="ctl-more" class="control-button" type="button" aria-label="More controls" aria-haspopup="menu" aria-expanded="false" aria-controls="overflow-menu">/
  );
  assert.match(indexSource, /<div id="overflow-menu" class="overflow-menu meeting-menu" role="menu" aria-label="More controls" hidden><\/div>/);
  // ⋯ is the last cell of `.controls-left`: the fixed controls end it, and Leave follows.
  assert.match(indexSource, /id="ctl-more"[\s\S]*?<\/div>\s*<\/div>\s*<div class="control-cell leave-cell">/);
  assert.match(pluginsSource, /controlsLeft\.insertBefore\(cell, controlsLeft\.querySelector\(':scope > \.overflow-cell'\)\)/);
  assert.doesNotMatch(pluginsSource, /controlsLeft\.appendChild\(cell\)/);
  // A plugin popover opened from the menu anchors to ⋯, not to its hidden button.
  assert.match(pluginsSource, /host\.activateButton\(button\.pluginId, button\.buttonId, controlAnchor\(btn\)\)/);
});
