import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import {
  HOLD_REFRESH_MS,
  HOLD_STALL_MS,
  HoldLastFrameState,
  frameIsUsable,
} from '../src/holdLastFrame.ts';

// These tests cover the DECISION logic only. They deliberately do NOT stand in
// for the rendered-pixel check: an event-level assertion cannot tell "held the
// last frame" from "went black quietly", which is exactly the trap CLAUDE.md's
// "Never show a black frame" rule calls out. The pixel evidence comes from
// `scripts/verify-no-black-frame.mjs`, which samples a real compositor.

test('frameIsUsable requires real dimensions and a decoded frame', () => {
  assert.equal(frameIsUsable({ videoWidth: 1280, videoHeight: 720, readyState: 2 }), true);
  assert.equal(frameIsUsable({ videoWidth: 0, videoHeight: 0, readyState: 2 }), false);
  assert.equal(frameIsUsable({ videoWidth: 1280, videoHeight: 720, readyState: 1 }), false);
  // The exact state a republished track leaves behind: element alive, no frame.
  assert.equal(frameIsUsable({ videoWidth: 0, videoHeight: 720, readyState: 4 }), false);
});

test('nothing is held before a first frame has ever been captured', () => {
  const state = new HoldLastFrameState();
  assert.equal(state.canHold, false);
  // A gap before any frame must NOT engage a hold -- there is nothing to hold,
  // and covering the video with an empty canvas would itself be a black frame.
  assert.equal(state.noteGap('source-swap'), false);
  assert.equal(state.isHolding, false);
  assert.equal(state.poll(10_000), false);
  assert.equal(state.isHolding, false);
});

test('the first usable frame is captured and later frames are rate limited', () => {
  const state = new HoldLastFrameState();
  assert.equal(state.noteFrame(1_000, true), true, 'first usable frame captures');
  assert.equal(state.canHold, true);
  assert.equal(state.noteFrame(1_000 + HOLD_REFRESH_MS - 1, true), false, 'inside refresh window');
  assert.equal(state.noteFrame(1_000 + HOLD_REFRESH_MS, true), true, 'refresh window elapsed');
});

test('an unusable frame never overwrites the held copy', () => {
  const state = new HoldLastFrameState();
  state.noteFrame(0, true);
  assert.equal(state.noteFrame(10_000, false), false);
  assert.equal(state.canHold, true, 'still has the earlier good frame to hold');
});

test('a known gap engages the hold immediately, with no watchdog latency', () => {
  const state = new HoldLastFrameState();
  state.noteFrame(0, true);
  assert.equal(state.noteGap('source-swap'), true);
  assert.equal(state.isHolding, true);
  assert.equal(state.holdReason, 'source-swap');
  // Idempotent: a second notice while already holding is not a state change.
  assert.equal(state.noteGap('muted'), false);
  assert.equal(state.holdReason, 'source-swap');
});

test('the watchdog engages the hold only after a real stall', () => {
  const state = new HoldLastFrameState();
  state.noteFrame(1_000, true);
  assert.equal(state.poll(1_000 + HOLD_STALL_MS - 1), false, 'normal jitter must not trip');
  assert.equal(state.isHolding, false);
  assert.equal(state.poll(1_000 + HOLD_STALL_MS), true);
  assert.equal(state.isHolding, true);
  assert.equal(state.holdReason, 'stall');
});

test('HOLD_STALL_MS sits above two frame intervals at 30fps and below a visible flash', () => {
  // Below 66ms it would trip on ordinary 30fps jitter; above ~150ms a
  // compositor that paints an empty video layer black would show a flash.
  assert.ok(HOLD_STALL_MS > 66, `HOLD_STALL_MS=${HOLD_STALL_MS} must exceed two 30fps intervals`);
  assert.ok(HOLD_STALL_MS <= 150, `HOLD_STALL_MS=${HOLD_STALL_MS} must stay imperceptible`);
});

test('the hold releases as soon as real frames resume', () => {
  const state = new HoldLastFrameState();
  state.noteFrame(0, true);
  state.noteGap('muted');
  assert.equal(state.isHolding, true);
  state.noteFrame(500, true);
  assert.equal(state.isHolding, false, 'a usable frame ends the hold');
  assert.equal(state.holdReason, null);
});

test('a full republish cycle holds continuously and never exposes an empty video', () => {
  // Replays the #627 sequence: steady frames, the sender republishes (source
  // swap -> no frame for ~300ms), then the new track starts decoding.
  const state = new HoldLastFrameState();
  let t = 0;
  for (let i = 0; i < 30; i += 1) {
    t += 33;
    state.noteFrame(t, true);
  }
  assert.equal(state.canHold, true);

  state.noteGap('source-swap');
  // Through the whole gap the hold stays engaged, including across watchdog
  // polls and the unusable frames the element reports while re-attaching.
  for (let i = 0; i < 10; i += 1) {
    t += 30;
    state.noteFrame(t, false);
    state.poll(t);
    assert.equal(state.isHolding, true, `still holding at +${t}ms into the gap`);
  }

  t += 30;
  state.noteFrame(t, true);
  assert.equal(state.isHolding, false, 'released once the replacement track decodes');
});

test('the full-range canvas renderer is still gated on readyState, which is why it is exempt', () => {
  // `tiles.ts` skips hold-last-frame for full-range shares because that path
  // already holds its last frame: the canvas retains its pixels and the render
  // loop refuses to draw when the video has nothing decoded. If that gate were
  // ever removed the loop would paint an empty video over the good frame and
  // full-range shares would start flashing black with no hold to catch them.
  // Assert the gate rather than trusting the comment that depends on it.
  const source = readFileSync(new URL('../src/tiles.ts', import.meta.url), 'utf8');
  const renderer = source.slice(source.indexOf('function startFullRangeRenderer'));
  const body = renderer.slice(0, renderer.indexOf('\n  function '));
  assert.match(
    body,
    /if \(width > 0 && height > 0 && video\.readyState >= 2\)/,
    'startFullRangeRenderer must keep drawing only when the video has a decoded frame'
  );
  assert.match(
    body,
    /context\.drawImage\(video, 0, 0, width, height\)/,
    'the guarded block is the one that paints the canvas'
  );
});

test('every CSS rule that repositions the share video also repositions the hold canvas', () => {
  // The held frame is a separate element stacked on the video, so it only looks
  // right while its box matches the video's box exactly. The remote-window
  // header pushes the video down 44px and the spotlight rail undoes that; if
  // the hold canvas is left out of either rule, a freeze renders the held frame
  // misaligned and covering the header. Enforce the invariant instead of
  // relying on whoever edits these selectors next remembering it.
  // Strip comments up front: a comment containing a brace would otherwise be
  // split across blocks and leak into the failure message.
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8').replace(
    /\/\*[\s\S]*?\*\//g,
    ''
  );
  const offenders: string[] = [];
  for (const block of css.split('}')) {
    const [selector, ...declarationParts] = block.split('{');
    if (declarationParts.length === 0) continue;
    const declarations = declarationParts.join('{');
    // Only rules that explicitly reposition the video vertically matter here;
    // the shared `inset: 0` base rule deliberately excludes the hold canvas so
    // the canvas never inherits its `background: #000`.
    if (!/(^|,|\s)\.tile[^,{]*\svideo\b/.test(selector)) continue;
    if (!/(^|;|\s)top\s*:/.test(declarations)) continue;
    if (selector.includes('share-hold-canvas')) continue;
    offenders.push(selector.trim().replace(/\s+/g, ' '));
  }
  assert.deepEqual(
    offenders,
    [],
    `these rules move .tile video without moving canvas.share-hold-canvas with it:\n${offenders.join('\n')}`
  );
});
