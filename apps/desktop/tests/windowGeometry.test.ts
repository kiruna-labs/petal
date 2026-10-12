import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  GALLERY_BREAKPOINT,
  GALLERY_MIN,
  GALLERY_PILL_HEIGHT,
  HOME_MIN,
  MAIN_WINDOW_GEOMETRY_KEY,
  MEETING_DEFAULT,
  MEETING_WINDOW_GEOMETRY_KEY,
  PILL_WINDOW_GEOMETRY_KEY,
  clampMainWindowSize,
  clampMeetingWindowSize,
  clampedPosition,
  galleryCollapsesToPill,
  loadMeetingWindowFrame,
  loadMainWindowSize,
  loadMeetingWindowSize,
  loadPillWindowFrame,
  loadWindowFrame,
  mainRouteEntryResizeTarget,
  monitorForWindowFrame,
  parseWindowSize,
  safeWindowPosition,
  saveMainWindowFrame,
  saveMainWindowSize,
  saveMeetingWindowFrame,
  saveMeetingWindowSize,
  savePillWindowFrame,
  windowIntersectsAnyWorkArea,
  type MonitorLike,
  type StorageLike
} from '../src/lib/data/windowGeometry.ts';

class MemoryStorage implements StorageLike {
  values = new Map<string, string>();

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

test('main and meeting sizes clamp to their route minimums', () => {
  assert.deepEqual(clampMainWindowSize({ width: 320.4, height: 500.2 }), HOME_MIN);
  // Below the minimum is clamped to it -- and a size inside the pill zone is
  // lifted to the pill height, so a restored gallery is never one the next
  // small drag collapses.
  assert.deepEqual(clampMeetingWindowSize({ width: 200.4, height: 120.2 }), { width: GALLERY_MIN.width, height: GALLERY_PILL_HEIGHT });
  assert.deepEqual(clampMeetingWindowSize({ width: 500, height: 300 }), { width: 500, height: GALLERY_PILL_HEIGHT });
  assert.equal(galleryCollapsesToPill(clampMeetingWindowSize({ width: 300, height: 200 })), false);
  assert.deepEqual(clampMeetingWindowSize({ width: 900, height: 120 }), { width: 900, height: GALLERY_MIN.height });
  assert.deepEqual(MEETING_DEFAULT, { width: 840, height: 560 });
  assert.deepEqual(clampMeetingWindowSize({ width: 800.4, height: 640.6 }), {
    width: 800,
    height: 641
  });
});

test('the gallery keeps a column of faces and a bar of faces; small both ways it is the pill', () => {
  // A column beside an editor and a bar along the top of the screen are
  // gallery shapes now (Gallery.svelte lays them out), not pill triggers.
  assert.deepEqual(clampMeetingWindowSize({ width: 300, height: 900 }), { width: 300, height: 900 });
  assert.deepEqual(clampMeetingWindowSize({ width: 1400, height: 220 }), { width: 1400, height: 220 });
  assert.ok(GALLERY_MIN.width < 300 && GALLERY_MIN.height < 220);
  assert.equal(galleryCollapsesToPill({ width: 300, height: 900 }), false, 'narrow alone is a column');
  assert.equal(galleryCollapsesToPill({ width: 1400, height: 170 }), false, 'short alone is a bar');
  assert.equal(galleryCollapsesToPill({ width: 840, height: 560 }), false);
  assert.equal(galleryCollapsesToPill({ width: GALLERY_BREAKPOINT - 2, height: GALLERY_PILL_HEIGHT - 1 }), true);
  assert.equal(galleryCollapsesToPill({ width: GALLERY_BREAKPOINT - 2, height: GALLERY_PILL_HEIGHT }), false);
  assert.equal(galleryCollapsesToPill({ width: GALLERY_BREAKPOINT - 1, height: GALLERY_MIN.height }), false);
});

test('main route entry keeps the current splash size unless it violates the minimum', () => {
  assert.equal(mainRouteEntryResizeTarget({ width: 400, height: 640 }), null);
  assert.equal(mainRouteEntryResizeTarget({ width: 480.4, height: 640.2 }), null);
  assert.deepEqual(mainRouteEntryResizeTarget({ width: 320.4, height: 500.2 }), HOME_MIN);
  assert.deepEqual(mainRouteEntryResizeTarget({ width: 400, height: 500.2 }), {
    width: 400,
    height: HOME_MIN.height
  });
});

test('parseWindowSize rejects malformed values and clamps valid values', () => {
  assert.equal(parseWindowSize(null, HOME_MIN), null);
  assert.equal(parseWindowSize('not json', HOME_MIN), null);
  assert.equal(parseWindowSize('{"width":0,"height":600}', HOME_MIN), null);
  assert.equal(parseWindowSize('{"width":500,"height":"600"}', HOME_MIN), null);
  assert.deepEqual(parseWindowSize('{"width":390.2,"height":550.8}', HOME_MIN), {
    width: 390,
    height: 560
  });
});

test('main and meeting geometry use separate storage keys', () => {
  const storage = new MemoryStorage();

  assert.equal(saveMainWindowSize({ width: 460, height: 620 }, storage), true);
  assert.equal(saveMeetingWindowSize({ width: 900, height: 700 }, storage), true);

  assert.equal(storage.getItem(MAIN_WINDOW_GEOMETRY_KEY), '{"width":460,"height":620}');
  assert.equal(storage.getItem(MEETING_WINDOW_GEOMETRY_KEY), '{"width":900,"height":700}');
  assert.deepEqual(loadMainWindowSize(storage), { width: 460, height: 620 });
  assert.deepEqual(loadMeetingWindowSize(storage), { width: 900, height: 700 });
});

test('frame storage preserves position while remaining size-compatible', () => {
  const storage = new MemoryStorage();

  assert.equal(saveMainWindowFrame({ width: 399.7, height: 601.2, x: -820.4, y: 44.8 }, storage), true);
  assert.equal(savePillWindowFrame({ width: 84.2, height: 76.8, x: 1440, y: 22 }, storage), true);

  assert.equal(
    storage.getItem(MAIN_WINDOW_GEOMETRY_KEY),
    '{"width":400,"height":601,"x":-820,"y":45}'
  );
  assert.equal(
    storage.getItem(PILL_WINDOW_GEOMETRY_KEY),
    '{"width":84,"height":77,"x":1440,"y":22}'
  );
  assert.deepEqual(loadMainWindowSize(storage), { width: 400, height: 601 });
  assert.deepEqual(loadPillWindowFrame(storage), { width: 84, height: 77, x: 1440, y: 22 });
});

test('meeting frame storage preserves gallery position and clamps to gallery minimum', () => {
  const storage = new MemoryStorage();

  assert.equal(
    saveMeetingWindowFrame({ width: 200.2, height: 150.4, x: 812.6, y: 96.3 }, storage),
    true
  );
  assert.equal(
    storage.getItem(MEETING_WINDOW_GEOMETRY_KEY),
    '{"width":240,"height":160,"x":813,"y":96}'
  );
  assert.deepEqual(loadMeetingWindowFrame(storage), { ...GALLERY_MIN, x: 813, y: 96 });
  assert.deepEqual(loadMeetingWindowSize(storage), GALLERY_MIN);
});

test('parseWindowFrame rejects missing coordinates and clamps valid frames', () => {
  assert.equal(loadWindowFrame('main', new MemoryStorage()), null);
  assert.equal(
    loadWindowFrame(
      'main',
      Object.assign(new MemoryStorage(), {
        values: new Map([[MAIN_WINDOW_GEOMETRY_KEY, '{"width":500,"height":600,"x":10}']])
      })
    ),
    null
  );

  const storage = new MemoryStorage();
  storage.setItem(MEETING_WINDOW_GEOMETRY_KEY, '{"width":200.2,"height":100.4,"x":9.6,"y":10.2}');
  assert.deepEqual(loadWindowFrame('meeting', storage), { ...GALLERY_MIN, x: 10, y: 10 });
});

test('stored undersized meeting geometry is recovered as a gallery-safe size', () => {
  const storage = new MemoryStorage();

  assert.equal(saveMeetingWindowSize({ width: 200, height: 100 }, storage), true);
  assert.deepEqual(loadMeetingWindowSize(storage), GALLERY_MIN);
});

test('storage failures are contained', () => {
  const throwingStorage: StorageLike = {
    getItem() {
      throw new Error('read failed');
    },
    setItem() {
      throw new Error('write failed');
    },
    removeItem() {
      throw new Error('remove failed');
    }
  };

  assert.equal(loadMainWindowSize(throwingStorage), null);
  assert.equal(saveMainWindowSize({ width: 500, height: 600 }, throwingStorage), false);
});

const monitors: MonitorLike[] = [
  {
    position: { x: 0, y: 0 },
    size: { width: 1440, height: 900 },
    workArea: { position: { x: 0, y: 25 }, size: { width: 1440, height: 875 } }
  },
  {
    position: { x: -1280, y: 0 },
    size: { width: 1280, height: 720 },
    workArea: { position: { x: -1280, y: 25 }, size: { width: 1280, height: 695 } }
  }
];

test('monitorForWindowFrame picks the containing monitor or nearest fallback', () => {
  assert.equal(
    monitorForWindowFrame({ x: -900, y: 120 }, { width: 300, height: 200 }, monitors),
    monitors[1]
  );
  assert.equal(
    monitorForWindowFrame({ x: 2000, y: 120 }, { width: 300, height: 200 }, monitors),
    monitors[0]
  );
});

test('safeWindowPosition clamps partially visible frames and recenters fully hidden frames', () => {
  assert.equal(windowIntersectsAnyWorkArea({ x: 1420, y: 100 }, { width: 240, height: 160 }, monitors), true);
  assert.deepEqual(safeWindowPosition({ x: 1420, y: 100 }, { width: 240, height: 160 }, monitors), {
    x: 1200,
    y: 100,
    changed: true,
    recentered: false
  });

  assert.equal(windowIntersectsAnyWorkArea({ x: 4000, y: 4000 }, { width: 240, height: 160 }, monitors), false);
  assert.deepEqual(safeWindowPosition({ x: 4000, y: 4000 }, { width: 240, height: 160 }, monitors), {
    x: 600,
    y: 383,
    changed: true,
    recentered: true
  });
});

test('clampedPosition keeps an oversized frame anchored inside the work area', () => {
  assert.deepEqual(clampedPosition({ x: 300, y: 300 }, { width: 2000, height: 1000 }, monitors[0]), {
    x: 0,
    y: 25,
    changed: true
  });
});
