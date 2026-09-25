import { test } from 'node:test';
import assert from 'node:assert/strict';

import { bindCameraFit, syncCameraFit } from '../src/cameraFit.ts';
import {
  containedMediaRect,
  mediaContentRect,
  mediaContentRectRelativeToTile,
  normalizedPointInContainedMedia,
  telepointerPosition,
} from '../src/telepointer.ts';

// #248: web camera tiles crop to fill their box within the shared caps
// (shared/logic/cameraCrop.ts) -- `data-fit="cover"` -- and letterbox past
// them. Every video-coordinate overlay must then map through the CROPPED
// picture, or a drawing on a camera tile lands off the face it was drawn on.

type Listener = () => void;

function fakeVideo(width: number, height: number) {
  const listeners = new Map<string, Listener[]>();
  return {
    videoWidth: width,
    videoHeight: height,
    isConnected: true,
    owner: null as unknown,
    dataset: {} as Record<string, string | undefined>,
    rect: { left: 0, top: 0, width: 0, height: 0 },
    listenerCount: () => [...listeners.values()].reduce((sum, list) => sum + list.length, 0),
    closest(selector: string) {
      return selector === '.tile' ? this.owner : null;
    },
    addEventListener(type: string, listener: Listener) {
      listeners.set(type, [...(listeners.get(type) ?? []), listener]);
    },
    fire(type: string) {
      for (const listener of listeners.get(type) ?? []) listener();
    },
    getBoundingClientRect() {
      return { ...this.rect, right: this.rect.left + this.rect.width, bottom: this.rect.top + this.rect.height };
    },
  };
}

function fakeTile(width: number, height: number, video: ReturnType<typeof fakeVideo>, left = 100, top = 50) {
  video.rect = { left, top, width, height };
  const tile = {
    clientWidth: width,
    clientHeight: height,
    querySelector: (selector: string) => (selector === 'video' ? video : null),
    getBoundingClientRect: () => ({ left, top, width, height, right: left + width, bottom: top + height }),
  };
  video.owner = tile;
  return tile;
}

/** A ResizeObserver stand-in that records what each instance observes. */
function installResizeObservers() {
  const originalRO = (globalThis as { ResizeObserver?: unknown }).ResizeObserver;
  const instances: Array<{ callback: () => void; targets: unknown[]; disconnected: boolean }> = [];
  Object.defineProperty(globalThis, 'ResizeObserver', {
    configurable: true,
    value: class {
      private readonly record: { callback: () => void; targets: unknown[]; disconnected: boolean };
      constructor(callback: () => void) {
        this.record = { callback, targets: [], disconnected: false };
        instances.push(this.record);
      }
      observe(target: unknown) {
        this.record.targets.push(target);
      }
      disconnect() {
        this.record.disconnected = true;
      }
    },
  });
  return {
    instances,
    live: () => instances.filter((instance) => !instance.disconnected),
    restore() {
      if (originalRO === undefined) delete (globalThis as { ResizeObserver?: unknown }).ResizeObserver;
      else Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, value: originalRO });
    },
  };
}

test('a 16:9 camera in a 4:3 tile covers; a 9:16 portrait camera in a 16:9 tile letterboxes', () => {
  const landscape = fakeVideo(1280, 720);
  assert.equal(syncCameraFit(fakeTile(400, 300, landscape) as unknown as HTMLElement, landscape as unknown as HTMLVideoElement), 'cover');
  assert.equal(landscape.dataset.fit, 'cover');

  const portrait = fakeVideo(720, 1280);
  assert.equal(syncCameraFit(fakeTile(480, 270, portrait) as unknown as HTMLElement, portrait as unknown as HTMLVideoElement), 'contain');
  assert.equal(portrait.dataset.fit, 'contain');
});

test('the fit is re-decided when the camera rotates or the tile is resized', () => {
  const observers = installResizeObservers();
  try {
    const video = fakeVideo(1280, 720);
    const tile = fakeTile(400, 300, video);
    bindCameraFit(video as unknown as HTMLVideoElement);
    assert.equal(video.dataset.fit, 'cover');
    assert.deepEqual(observers.live().map((o) => o.targets), [[tile]]);

    // The phone turns portrait: the stream's intrinsic size flips.
    video.videoWidth = 720;
    video.videoHeight = 1280;
    video.fire('resize');
    assert.equal(video.dataset.fit, 'contain');

    // The tile becomes a portrait spotlight hero: 9:16 now covers it.
    tile.clientWidth = 270;
    tile.clientHeight = 480;
    observers.live()[0]!.callback();
    assert.equal(video.dataset.fit, 'cover');

    // Binding again (a new track on the same element) re-decides without
    // stacking listeners or observers.
    const listeners = video.listenerCount();
    bindCameraFit(video as unknown as HTMLVideoElement);
    assert.equal(video.listenerCount(), listeners);
    assert.equal(observers.live().length, 1);
  } finally {
    observers.restore();
  }
});

test('a simulcast layer switch (same aspect, fewer pixels) keeps the decision', () => {
  const observers = installResizeObservers();
  try {
    const video = fakeVideo(1280, 720);
    fakeTile(300, 253, video); // ~7:6, the narrow end of the packer range
    bindCameraFit(video as unknown as HTMLVideoElement);
    assert.equal(video.dataset.fit, 'cover');
    video.videoWidth = 640;
    video.videoHeight = 360;
    video.fire('resize');
    assert.equal(video.dataset.fit, 'cover');
  } finally {
    observers.restore();
  }
});

test('the fit follows the tile the video is in now, and stops observing once it leaves', () => {
  const observers = installResizeObservers();
  try {
    const video = fakeVideo(1280, 720);
    const first = fakeTile(400, 300, video);
    bindCameraFit(video as unknown as HTMLVideoElement);
    assert.deepEqual(observers.live().map((o) => o.targets), [[first]]);

    // Re-parented into a portrait box: the old tile is released, the new one
    // observed, and the decision uses the new box.
    const second = fakeTile(200, 400, video);
    bindCameraFit(video as unknown as HTMLVideoElement);
    assert.deepEqual(observers.live().map((o) => o.targets), [[second]]);
    assert.equal(video.dataset.fit, 'contain');

    // The tile is removed (participant left): the next notification finds the
    // video detached and disconnects instead of holding the tile.
    video.isConnected = false;
    observers.live()[0]!.callback();
    assert.equal(observers.live().length, 0);
  } finally {
    observers.restore();
  }
});

test('draw capture and render on a cropped camera map through the cropped picture', () => {
  // A 16:9 camera cropped into a 4:3 tile: 1/8 of the picture's width is off
  // each side, so the tile's left edge shows picture x = 0.125.
  const video = fakeVideo(1280, 720);
  const tile = fakeTile(400, 300, video);
  syncCameraFit(tile as unknown as HTMLElement, video as unknown as HTMLVideoElement);

  const { bounds, media } = mediaContentRect(tile as unknown as HTMLDivElement);
  assert.ok(Math.abs(bounds.width - 1600 / 3) < 1e-9, 'the picture is wider than the tile');
  const leftEdge = normalizedPointInContainedMedia(bounds, media, { x: 100, y: 200 }, { clamp: false })!;
  assert.ok(Math.abs(leftEdge.x - 0.125) < 1e-9, `left edge maps to ${leftEdge.x}`);
  const centre = normalizedPointInContainedMedia(bounds, media, { x: 300, y: 200 })!;
  assert.ok(Math.abs(centre.x - 0.5) < 1e-9 && Math.abs(centre.y - 0.5) < 1e-9);

  // Receive side (drawDisplay/telepointerDisplay): that same picture point
  // lands back on the tile's left edge, in tile coordinates.
  const relative = mediaContentRectRelativeToTile(tile as unknown as HTMLDivElement);
  const content = containedMediaRect(relative.bounds, relative.media);
  assert.ok(Math.abs(content.left + content.width * 0.125) < 1e-9);
  const pointer = telepointerPosition(relative.bounds, relative.media, { x: 0.5, y: 0.5 });
  assert.ok(Math.abs(pointer.x - 200) < 1e-9 && Math.abs(pointer.y - 150) < 1e-9);

  // Mutation guard: the same click on a letterboxed (contain) camera is the
  // picture's own edge -- so the mapping above really comes from data-fit.
  video.dataset.fit = 'contain';
  const contained = mediaContentRect(tile as unknown as HTMLDivElement);
  const edge = normalizedPointInContainedMedia(contained.bounds, contained.media, { x: 100, y: 200 })!;
  assert.equal(edge.x, 0);
});
