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
  createSerializedHoverTabCommandQueue,
  hoverTabPositionForSide,
  isHoverTabDragging,
  moveHoverTabGesture,
  offerHoverTabPreview,
  projectHoverTabCenter,
  projectHoverTabCenterWithSide,
  settleHoverTabPreview,
  takeHoverTabPreview
} from '../src/lib/hoverTabDrag.ts';

const __dirname = dirname(fileURLToPath(import.meta.url));
const hoverTabSource = readFileSync(resolve(__dirname, '../src/routes/hover-tab/+page.svelte'), 'utf8');
const geometryFixture = JSON.parse(
  readFileSync(resolve(__dirname, 'fixtures/hover-tab-geometry.json'), 'utf8')
) as {
  frame: { x: number; y: number; width: number; height: number };
  cases: Array<{ position: number; center: { x: number; y: number } }>;
};
const sourceFrame = geometryFixture.frame;
const rightTab = { x: 800, y: 330 };

function pointerForCenter(x: number, y: number) {
  return { x, y };
}

test('invalid pointer or source frames fail closed instead of teleporting', () => {
  assert.equal(projectHoverTabCenter({ x: Number.NaN, y: 100 }, sourceFrame), null);
  assert.equal(projectHoverTabCenter({ x: 100, y: 100 }, { ...sourceFrame, width: 0 }), null);
  assert.equal(projectHoverTabCenter({ x: 100, y: 100 }, { ...sourceFrame, height: -1 }), null);
});

test('hover-tab movement below six pixels stays a primary click', () => {
  const gesture = beginHoverTabGesture(7, 820, 350, 3 / 8, rightTab.x, rightTab.y, sourceFrame);
  const moved = moveHoverTabGesture(gesture, 820 + HOVER_TAB_DRAG_THRESHOLD_PX - 0.01, 350);
  assert.equal(moved.started, false);
  assert.equal(moved.position, null);
  assert.equal(isHoverTabDragging(moved.gesture), false);
});

test('hover-tab movement at the threshold starts a drag and preserves the grab point', () => {
  const gesture = beginHoverTabGesture(7, 820, 350, 3 / 8, rightTab.x, rightTab.y, sourceFrame);
  const started = moveHoverTabGesture(gesture, 820, 350 + HOVER_TAB_DRAG_THRESHOLD_PX);
  assert.equal(started.started, true);
  assert.equal(started.gesture.phase, 'dragging');
  assert.equal(isHoverTabDragging(started.gesture), true);
  assert.ok((started.position ?? 0) > 3 / 8);
});

test('shared geometry fixture matches the frontend perimeter projection', () => {
  for (const { position, center } of geometryFixture.cases) {
    const projected = projectHoverTabCenter(center, sourceFrame);
    assert.notEqual(projected, null);
    assert.ok(Math.abs(projected - position) < 0.000001);
  }
});

test('the canonical perimeter reaches all four centered edges', () => {
  assert.equal(hoverTabPositionForSide('top'), 1 / 8);
  assert.equal(hoverTabPositionForSide('right'), 3 / 8);
  assert.equal(hoverTabPositionForSide('bottom'), 5 / 8);
  assert.equal(hoverTabPositionForSide('left'), 7 / 8);

  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(550, 180), sourceFrame) - 1 / 8) < 0.000001);
  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(820, 350), sourceFrame) - 3 / 8) < 0.000001);
  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(550, 520), sourceFrame) - 5 / 8) < 0.000001);
  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(280, 350), sourceFrame) - 7 / 8) < 0.000001);
});

test('corner projection stops at one edge before switching to the next', () => {
  const beforeTopRight = projectHoverTabCenter(pointerForCenter(776, 180), sourceFrame);
  const corner = projectHoverTabCenter(pointerForCenter(820, 224), sourceFrame);
  const afterTopRight = projectHoverTabCenter(pointerForCenter(820, 240), sourceFrame);
  assert.ok(Math.abs(beforeTopRight - 1 / 4) < 0.000001);
  assert.ok(Math.abs(corner - 1 / 4) < 0.000001);
  assert.ok(afterTopRight > corner);
  assert.ok(afterTopRight < 3 / 8);
});

test('both left corners use the bounded cardinal edge spans', () => {
  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(280, 224), sourceFrame) - 0) < 0.000001);
  assert.ok(Math.abs(projectHoverTabCenter(pointerForCenter(280, 476), sourceFrame) - 3 / 4) < 0.000001);
});

test('corner hysteresis retains the current edge until the crossing is deliberate', () => {
  const near = projectHoverTabCenterWithSide(pointerForCenter(800, 202), sourceFrame, 'top');
  const crossed = projectHoverTabCenterWithSide(pointerForCenter(800, 210), sourceFrame, 'top');
  assert.equal(near?.side, 'top');
  assert.equal(crossed?.side, 'right');
});

test('hover-tab drag follows absolute screen samples without a client-coordinate feedback loop', () => {
  const gesture = beginHoverTabGesture(7, 820, 350, 3 / 8, rightTab.x, rightTab.y, sourceFrame);
  let current = gesture;
  for (const y of [360, 370, 380, 390, 400, 410, 420, 430, 440, 450]) {
    const moved = moveHoverTabGesture(current, 820, y);
    current = moved.gesture;
    assert.ok(moved.position !== null);
  }
  const final = moveHoverTabGesture(current, 820, 450).position ?? 0;
  assert.ok(Math.abs(final - (1 / 4 + 226 / (4 * 252))) < 0.000001);

  // The pointer sample is global and remains tied to the original grab point;
  // it does not subtract the native tab's latest displacement.
  const localLike = moveHoverTabGesture(gesture, 820, 450).position ?? 0;
  assert.equal(final, localLike);
});

test('the real hover-tab route uses canonical positions and global screen coordinates', () => {
  assert.match(
    hoverTabSource,
    /beginHoverTabGesture\(\s*event\.pointerId,\s*event\.screenX,\s*event\.screenY/
  );
  assert.match(
    hoverTabSource,
    /moveHoverTabGesture\(gesture,\s*event\.screenX,\s*event\.screenY/
  );
  assert.match(hoverTabSource, /currentFrame/);
  assert.match(hoverTabSource, /dragGesture\.sourceFrame = frame/);
  assert.match(hoverTabSource, /dragGesture\.sourceFrame = frame;[\s\S]*attachment = update\.attachment/);
  assert.match(hoverTabSource, /validWindowFrame\(frame\)/);
  assert.match(hoverTabSource, /perimeterPosition/);
  assert.match(hoverTabSource, /perimeterPosition: position/);
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

test('serialized native commands defer later work and isolate cancellation failures', async () => {
  const calls: string[] = [];
  let releaseFirst!: () => void;
  const first = new Promise<number>((resolve) => {
    releaseFirst = () => resolve(1);
  });
  const queue = createSerializedHoverTabCommandQueue<string, number>(async (command) => {
    calls.push(command);
    return command === 'first' ? first : 2;
  });

  const firstResult = queue('first');
  const secondResult = queue('second');
  await Promise.resolve();
  assert.deepEqual(calls, ['first']);
  releaseFirst();
  assert.equal(await firstResult, 1);
  assert.equal(await secondResult, 2);
  assert.deepEqual(calls, ['first', 'second']);

  const rejectingQueue = createSerializedHoverTabCommandQueue<string, number>(async (command) => {
    if (command === 'cancel') throw new Error('stale cancellation');
    return 3;
  });
  await assert.rejects(rejectingQueue('cancel'));
  assert.equal(await rejectingQueue('commit'), 3);
});

test('latest hover-tab previews stay bounded and deliver only the newest sample', () => {
  const preview = createHoverTabPreviewState();
  assert.equal(offerHoverTabPreview(preview, 0.1), 0.1);
  assert.equal(preview.inFlight, true);

  assert.equal(offerHoverTabPreview(preview, 0.2), null);
  assert.equal(offerHoverTabPreview(preview, 0.8), null);
  assert.equal(preview.pendingPosition, 0.8);

  const newest = settleHoverTabPreview(preview);
  assert.equal(newest, 0.8);
  assert.equal(preview.inFlight, false);
  preview.pendingPosition = newest;
  assert.equal(offerHoverTabPreview(preview, takeHoverTabPreview(preview)!), 0.8);
  assert.equal(preview.inFlight, true);

  assert.equal(offerHoverTabPreview(preview, 1.1), null);
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

test('hover-tab gesture keeps pointer identity and restores its original perimeter position', () => {
  const gesture = beginHoverTabGesture(42, 820, 350, 0.25, rightTab.x, rightTab.y, sourceFrame);
  const moved = moveHoverTabGesture(gesture, 820, 430);
  assert.equal(moved.gesture.pointerId, 42);
  assert.ok(moved.position !== null);
  assert.equal(cancelHoverTabGesture(moved.gesture), 0.25);
});
