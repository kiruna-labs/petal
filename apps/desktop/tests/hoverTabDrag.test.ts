import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  HOVER_TAB_DRAG_THRESHOLD_PX,
  beginHoverTabGesture,
  cancelHoverTabGesture,
  clearHoverTabPreview,
  createHoverTabPreviewState,
  isHoverTabDragging,
  moveHoverTabGesture,
  offerHoverTabPreview,
  settleHoverTabPreview,
  takeHoverTabPreview
} from '../src/lib/hoverTabDrag.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const hoverTabSource = readFileSync(resolve(__dirname, '../src/routes/hover-tab/+page.svelte'), 'utf8');

test('hover-tab movement below six pixels stays a primary click', () => {
  const gesture = beginHoverTabGesture(7, 10, 20, 0.5);
  const moved = moveHoverTabGesture(gesture, 10 + HOVER_TAB_DRAG_THRESHOLD_PX - 0.01, 20, 300);
  assert.equal(moved.started, false);
  assert.equal(moved.offset, null);
  assert.equal(isHoverTabDragging(moved.gesture), false);
});

test('hover-tab movement at the threshold starts a vertical drag and clamps its offset', () => {
  const gesture = beginHoverTabGesture(7, 10, 20, 0.5);
  const started = moveHoverTabGesture(gesture, 10, 20 + HOVER_TAB_DRAG_THRESHOLD_PX, 300);
  assert.equal(started.started, true);
  assert.equal(started.gesture.phase, 'dragging');
  assert.equal(isHoverTabDragging(started.gesture), true);
  assert.ok((started.offset ?? 0) > 0.5);

  const top = moveHoverTabGesture(started.gesture, 10, -1000, 300);
  const bottom = moveHoverTabGesture(started.gesture, 10, 1000, 300);
  assert.equal(top.offset, 0);
  assert.equal(bottom.offset, 1);
});

test('hover-tab drag preserves 1:1 movement when the native surface follows the pointer', () => {
  const sourceHeight = 300;
  const tabHeight = 40;
  const travel = sourceHeight - tabHeight;
  const pointerStartScreenY = 1000;
  let screenGesture = beginHoverTabGesture(7, 200, pointerStartScreenY, 0.5);
  let screenTabDelta = 0;

  for (const pointerDelta of [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]) {
    const moved = moveHoverTabGesture(
      screenGesture,
      200,
      pointerStartScreenY + pointerDelta,
      sourceHeight,
      tabHeight
    );
    screenGesture = moved.gesture;
    screenTabDelta = (moved.offset! - 0.5) * travel;
  }

  assert.ok(Math.abs(screenTabDelta - 100) < 0.000001);

  // This is the feedback loop caused by clientY: once the native tab moves,
  // each later local sample loses the tab's own displacement.
  let localGesture = beginHoverTabGesture(7, 20, 20, 0.5);
  let localTabDelta = 0;
  for (const pointerDelta of [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]) {
    const localPointerY = 20 + pointerDelta - localTabDelta;
    const moved = moveHoverTabGesture(localGesture, 20, localPointerY, sourceHeight, tabHeight);
    localGesture = moved.gesture;
    localTabDelta = (moved.offset! - 0.5) * travel;
  }

  assert.ok(localTabDelta < 100);
});

test('the real hover-tab route measures movement in stable global screen coordinates', () => {
  assert.match(
    hoverTabSource,
    /beginHoverTabGesture\(\s*event\.pointerId,\s*event\.screenX,\s*event\.screenY/
  );
  assert.match(
    hoverTabSource,
    /moveHoverTabGesture\(\s*gesture,\s*event\.screenX,\s*event\.screenY/
  );
  assert.doesNotMatch(hoverTabSource, /beginHoverTabGesture\(\s*event\.pointerId,\s*event\.client/);
  assert.doesNotMatch(hoverTabSource, /moveHoverTabGesture\(\s*gesture,\s*event\.client/);
});

test('the real hover-tab route captures pointers, throttles updates, and cancels safely', () => {
  assert.match(hoverTabSource, /onpointerdown=\{onActionPointerDown\}/);
  assert.match(hoverTabSource, /onpointermove=\{onActionPointerMove\}/);
  assert.match(hoverTabSource, /onpointerup=\{onActionPointerUp\}/);
  assert.match(hoverTabSource, /onpointercancel=\{onActionPointerCancel\}/);
  assert.match(hoverTabSource, /onlostpointercapture=\{onActionLostPointerCapture\}/);
  assert.match(hoverTabSource, /setPointerCapture/);
  assert.match(hoverTabSource, /requestAnimationFrame\(flushDragUpdate\)/);
  assert.match(hoverTabSource, /cancelAnimationFrame/);
  assert.match(hoverTabSource, /suppressNextClick/);
  assert.match(hoverTabSource, /event\.key === 'Escape'/);
  assert.match(hoverTabSource, /const unHide = listen\(EVENTS\.hoverTabHide[\s\S]*cancelActionDrag\(\)/);
  assert.match(hoverTabSource, /enqueueHoverTabDrag\('cancel'/);
  assert.match(hoverTabSource, /class:dragging=\{isDragging\}/);
});

test('latest hover-tab previews stay bounded and deliver only the newest sample', () => {
  const preview = createHoverTabPreviewState();
  assert.equal(offerHoverTabPreview(preview, 0.1), 0.1);
  assert.equal(preview.inFlight, true);

  assert.equal(offerHoverTabPreview(preview, 0.2), null);
  assert.equal(offerHoverTabPreview(preview, 0.8), null);
  assert.equal(preview.pendingOffset, 0.8);

  const newest = settleHoverTabPreview(preview);
  assert.equal(newest, 0.8);
  assert.equal(preview.inFlight, false);
  preview.pendingOffset = newest;
  assert.equal(offerHoverTabPreview(preview, takeHoverTabPreview(preview)!), 0.8);
  assert.equal(preview.inFlight, true);

  // A terminal phase drops an unsent successor but lets the active command
  // settle normally, so commit/cancel can remain behind it in the route queue.
  assert.equal(offerHoverTabPreview(preview, 0.9), null);
  clearHoverTabPreview(preview);
  assert.equal(settleHoverTabPreview(preview), null);
  assert.equal(preview.inFlight, false);
});

test('the route keeps preview IPC latest-wins and terminal phases ordered', () => {
  assert.match(hoverTabSource, /enqueueHoverTabDrag\('update'[\s\S]*\.finally\(\(\) => \{[\s\S]*settleHoverTabPreview/);
  assert.match(hoverTabSource, /dragPreviewState !== preview/);
  assert.match(hoverTabSource, /enqueueHoverTabDrag\('commit'/);
  assert.match(hoverTabSource, /enqueueHoverTabDrag\('cancel'/);
  assert.match(
    hoverTabSource,
    /enqueueHoverTabDrag\('update'[\s\S]*\.catch\(\(\) => \{[\s\S]*cancelActionDrag\(\)/
  );
});

test('hover-tab gesture keeps pointer identity and uses source-relative travel', () => {
  const gesture = beginHoverTabGesture(42, 100, 200, 0.25);
  const moved = moveHoverTabGesture(gesture, 100, 330, 300, 40);
  assert.equal(moved.gesture.pointerId, 42);
  assert.equal(moved.offset, 0.75);
  assert.equal(cancelHoverTabGesture(moved.gesture), 0.25);
});
