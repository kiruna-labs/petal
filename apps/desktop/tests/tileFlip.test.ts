import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  uniformFlip,
  uniformFlipClipPath,
  uniformFlipKeyframes,
  uniformFlipTransform,
  visibleFlipRect,
  type FlipRect,
  type UniformFlip
} from '@petal/shared/logic/tileFlip';
import { uniformTileFlip, uniformTileFlipInFlight } from '../src/lib/motion.ts';

// #204 kept tiles at 16:9 partly because a shape change makes the classic
// FLIP `scale(sx, sy)` non-uniform, squashing live video for the length of the
// move. #248 lets camera tiles change shape, so the FLIP frame is now one
// uniform scale plus a clip of the box. These pin both halves of that.

/** Where the first frame actually paints: the laid-out `next` box, clipped by
 * the insets, then scaled and translated from its top-left. */
function paintedFirstFrame(next: FlipRect, flip: UniformFlip): FlipRect {
  return {
    left: next.left + flip.dx + flip.scale * flip.insetX,
    top: next.top + flip.dy + flip.scale * flip.insetY,
    width: flip.scale * (next.width - 2 * flip.insetX),
    height: flip.scale * (next.height - 2 * flip.insetY)
  };
}

function assertRectClose(actual: FlipRect, expected: FlipRect) {
  for (const key of ['left', 'top', 'width', 'height'] as const) {
    assert.ok(Math.abs(actual[key] - expected[key]) < 1e-6, `${key}: ${actual[key]} vs ${expected[key]}`);
  }
}

const UNIFORM_SCALE = /scale\((-?[\d.]+)\)$/;

test('a same-shape move is a plain uniform FLIP with no clip', () => {
  const previous = { left: 0, top: 0, width: 240, height: 135 };
  const next = { left: 100, top: 50, width: 480, height: 270 };
  const flip = uniformFlip(previous, next)!;
  assert.equal(flip.scale, 0.5);
  assert.equal(flip.insetX, 0);
  assert.equal(flip.insetY, 0);
  assert.equal(uniformFlipClipPath(flip), null, 'no clip, so the speaking ring is never clipped on a plain move');
  assertRectClose(paintedFirstFrame(next, flip), previous);
});

test('a shape change keeps one uniform scale and clips the box to exactly the old rect', () => {
  // Two cameras on a landscape phone (7:5-ish) -> three (7:6): the tile gets
  // narrower relative to its height.
  const previous = { left: 10, top: 20, width: 391, height: 280 };
  const next = { left: 12, top: 60, width: 258, height: 218 };
  const flip = uniformFlip(previous, next)!;
  assert.equal(flip.scale, Math.max(391 / 258, 280 / 218));
  assert.equal(flip.insetX, 0);
  assert.ok(flip.insetY > 0, 'the taller-than-needed axis is clipped');
  assertRectClose(paintedFirstFrame(next, flip), previous);

  const [first, last] = uniformFlipKeyframes(flip, 16);
  assert.match(first.transform, UNIFORM_SCALE, 'never scale(x, y)');
  assert.equal(Number(UNIFORM_SCALE.exec(first.transform)![1]), Number(flip.scale.toFixed(5)));
  assert.equal(first.transformOrigin, 'top left');
  assert.match(first.clipPath ?? '', /^inset\([\d.]+px 0px [\d.]+px 0px round 16px\)$/);
  assert.equal(last.transform, 'translate(0px, 0px) scale(1)');
  assert.equal(last.clipPath, 'inset(0px 0px 0px 0px round 16px)');
});

test('growing wider clips the sides instead', () => {
  const previous = { left: 0, top: 0, width: 300, height: 253 }; // ~7:6
  const next = { left: 0, top: 0, width: 480, height: 270 }; // 16:9
  const flip = uniformFlip(previous, next)!;
  assert.ok(flip.insetX > 0);
  assert.equal(flip.insetY, 0);
  assertRectClose(paintedFirstFrame(next, flip), previous);
});

test('a still tile or an unmeasurable rect has nothing to animate', () => {
  const rect = { left: 5, top: 5, width: 200, height: 150 };
  assert.equal(uniformFlip(rect, { ...rect, left: 5.2 }), null);
  assert.equal(uniformFlip(rect, { ...rect, width: 0 }), null);
  assert.equal(uniformFlip({ ...rect, height: Number.NaN }, rect), null);
});

test('the mid-flight frame blends translate, scale and inset together', () => {
  const flip = uniformFlip({ left: 0, top: 0, width: 400, height: 300 }, { left: 40, top: 0, width: 200, height: 200 })!;
  assert.equal(uniformFlipTransform(flip, 0), 'translate(0px, 0px) scale(1)');
  const half = uniformFlipTransform(flip, 0.5);
  assert.match(half, UNIFORM_SCALE);
  assert.equal(Number(UNIFORM_SCALE.exec(half)![1]), Number((1 + (flip.scale - 1) / 2).toFixed(5)));
  assert.equal(uniformFlipClipPath(flip, 0.5), `inset(${flip.insetY / 2}px 0px ${flip.insetY / 2}px 0px)`);
});

test('an interrupted FLIP retargets from its visible, clipped box', () => {
  const painted = { left: 100, top: 50, width: 400, height: 300 };
  // Layout width 200 -> painted at 2x; a 10px/20px local clip is 20px/40px on screen.
  assertRectClose(visibleFlipRect(painted, 200, 'inset(10px 20px round 16px)'), {
    left: 140,
    top: 70,
    width: 320,
    height: 260
  });
  assertRectClose(visibleFlipRect(painted, 200, 'inset(10px 20px 30px 40px)'), {
    left: 180,
    top: 70,
    width: 280,
    height: 220
  });
  assert.deepEqual(visibleFlipRect(painted, 200, 'none'), painted);
  assert.deepEqual(visibleFlipRect(painted, 0, 'inset(10px)'), painted);
});

test('a keyed-list FLIP registers its node as in flight for exactly its duration', () => {
  const node = {} as Element;
  const from = { left: 0, top: 0, width: 391, height: 280 } as DOMRect;
  const to = { left: 0, top: 0, width: 258, height: 218 } as DOMRect;
  assert.equal(uniformTileFlipInFlight(node), false);
  const started = performance.now();
  uniformTileFlip(node, { from, to }, { duration: 220 });
  assert.equal(uniformTileFlipInFlight(node, started + 100), true);
  assert.equal(uniformTileFlipInFlight(node, started + 221), false);
  // A move with nothing to animate never marks the node.
  const still = {} as Element;
  uniformTileFlip(still, { from, to: from }, { duration: 220 });
  assert.equal(uniformTileFlipInFlight(still), false);
});

test("the desktop keyed-list FLIP is the uniform one, not svelte/animate's flip", () => {
  const from = { left: 0, top: 0, width: 391, height: 280 } as DOMRect;
  const to = { left: 0, top: 0, width: 258, height: 218 } as DOMRect;
  const config = uniformTileFlip({} as Element, { from, to }, { duration: 220 });
  assert.equal(config.duration, 220);
  const css = config.css!(0, 1);
  assert.match(css, /transform: translate\([^)]*\) scale\([\d.]+\);/);
  assert.match(css, /transform-origin: top left;/);
  assert.match(css, /clip-path: inset\(/);
  assert.equal(uniformTileFlip({} as Element, { from, to: from }, { duration: 220 }).duration, 0);

  const gallery = readFileSync(new URL('../src/lib/components/Gallery.svelte', import.meta.url), 'utf8');
  assert.doesNotMatch(gallery, /from 'svelte\/animate'/);
  // An interrupting layout pass retargets a Svelte-owned (keyed-list) FLIP
  // from its visible, clipped box too -- not only this file's WAAPI ones.
  assert.match(
    gallery,
    /const inFlight = \(key && activeGalleryTileAnimations\.has\(key\)\) \|\| uniformTileFlipInFlight\(tile\);/
  );
  assert.match(gallery, /inFlight\s*\? visibleFlipRect\(painted, tile\.offsetWidth, getComputedStyle\(tile\)\.clipPath\)/);
  assert.match(gallery, /animate:uniformTileFlip=\{\{ duration: suppressSvelteFlip \? 0 : tileLayoutDuration\(\) \}\}/);
  assert.match(gallery, /uniformFlipKeyframes\(flip, radius\)/);
  assert.doesNotMatch(gallery, /scale\(\$\{scaleX\}, \$\{scaleY\}\)/);
});
