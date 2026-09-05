import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  DRAW_FLUSH_MS,
  DRAW_UNAVAILABLE_LABEL,
  MAX_DRAW_POINTS_PER_MESSAGE,
  chunkDrawPoints,
  createDrawMessageBuilder,
  drawControlCopy,
  drawTargetFromTile,
  hasDrawableTarget,
  penCursor,
  pointForTile,
} from '../src/drawSender.ts';
import { MAX_DRAW_TEXT_CHARS } from '../src/draw.ts';

/** #892: a tile whose <video> rect differs from the tile's own rect -- the
 * header-inset shape a real share tile has. A fixture where the two rects
 * coincide (as a naive fake tile would default to) cannot catch a capture
 * that silently fell back to normalizing against the tile. */
class FakeTileWithVideoAt {
  private readonly tileRect: { left: number; top: number; width: number; height: number };
  private readonly video: { rect: { left: number; top: number; width: number; height: number }; width: number; height: number } | null;

  constructor(
    tileRect: { left: number; top: number; width: number; height: number },
    video: { rect: { left: number; top: number; width: number; height: number }; width: number; height: number } | null
  ) {
    this.tileRect = tileRect;
    this.video = video;
  }

  querySelector<T extends Element>(selector: string): T | null {
    if (selector !== 'video' || !this.video) return null;
    const { rect, width, height } = this.video;
    return { videoWidth: width, videoHeight: height, getBoundingClientRect: () => rect as DOMRect } as unknown as T;
  }

  getBoundingClientRect() {
    return this.tileRect as DOMRect;
  }
}

test('pointForTile normalizes the real capture path against the video content box, not the tile (#892 header-inset regression)', () => {
  // Tile 400x300 at viewport (50,20) -- off-origin so absolute/relative helper confusion cannot cancel out; video inset top:45/left:1 (44px docked
  // header + 1px border), sized so its OWN aspect exactly matches the media
  // aspect (400x225 box vs 1600x900 media, both 16:9) -- isolates the
  // rect-CHOICE bug from the separate letterbox/pillarbox math already
  // covered in telepointer.test.ts.
  const tile = new FakeTileWithVideoAt(
    { left: 50, top: 20, width: 400, height: 300 },
    { rect: { left: 51, top: 65, width: 400, height: 225 }, width: 1600, height: 900 }
  );

  const topLeft = pointForTile(tile as unknown as HTMLDivElement, { clientX: 51, clientY: 65 });
  assert.deepEqual(topLeft, { x: 0, y: 0 }, 'video top-left corner must normalize to (0,0)');

  const center = pointForTile(tile as unknown as HTMLDivElement, { clientX: 251, clientY: 177.5 });
  assert.deepEqual(center, { x: 0.5, y: 0.5 }, 'video center must normalize to (0.5,0.5)');

  const bottomRight = pointForTile(tile as unknown as HTMLDivElement, { clientX: 451, clientY: 290 });
  assert.deepEqual(bottomRight, { x: 1, y: 1 }, 'video bottom-right corner must normalize to (1,1)');

  // A click inside the header band (above the video's top edge, y=45) is
  // OUTSIDE the video content box and must clamp to y=0, not land at some
  // fraction into the content the way normalizing against the full tile
  // rect (starting at y=0) would.
  const insideHeader = pointForTile(tile as unknown as HTMLDivElement, { clientX: 251, clientY: 40 });
  assert.deepEqual(insideHeader, { x: 0.5, y: 0 }, 'a header click must clamp to the video top edge');
});

test('pointForTile falls back to the tile rect when the tile has no video (camera/placeholder tiles)', () => {
  const tile = new FakeTileWithVideoAt({ left: 50, top: 20, width: 400, height: 300 }, null);
  // No <video> -> zero media size -> containedMediaRect falls back to the
  // bounds (the tile rect) unchanged, so this exercises the `(video ?? tile)`
  // fallback itself, not a letterbox computation.
  assert.deepEqual(pointForTile(tile as unknown as HTMLDivElement, { clientX: 250, clientY: 170 }), { x: 0.5, y: 0.5 });
  assert.deepEqual(pointForTile(tile as unknown as HTMLDivElement, { clientX: 450, clientY: 320 }), { x: 1, y: 1 });
});

test('draw sender batches points at the MVP cadence and chunks large payloads', () => {
  assert.equal(DRAW_FLUSH_MS, 50);
  assert.ok(DRAW_FLUSH_MS >= 50 && DRAW_FLUSH_MS <= 100);

  const points = Array.from({ length: MAX_DRAW_POINTS_PER_MESSAGE + 2 }, (_, index) => ({
    x: index / MAX_DRAW_POINTS_PER_MESSAGE,
    y: 0.5,
  }));
  const chunks = chunkDrawPoints(points);

  assert.equal(chunks.length, 2);
  assert.equal(chunks[0].length, MAX_DRAW_POINTS_PER_MESSAGE);
  assert.equal(chunks[1].length, 2);

  const builder = createDrawMessageBuilder({ createStrokeId: () => 'stroke-batch' });
  const begin = builder.begin({ ownerIdentity: 'native-1', windowId: 42 }, { x: 0, y: 0 });
  const messages = builder.points(begin, points);

  assert.equal(messages.length, 2);
  assert.ok(messages.every((message) => message.type === 'points'));
  assert.ok(messages.every((message) => message.points.length <= MAX_DRAW_POINTS_PER_MESSAGE));
});

test('draw target resolution accepts share and camera drawable surfaces without making cameras window targets', () => {
  const shareTile = { dataset: { owner: 'native-1', windowId: '42' } } as unknown as HTMLDivElement;
  assert.deepEqual(drawTargetFromTile(shareTile), { ownerIdentity: 'native-1', windowId: 42 });

  const cameraTile = {
    dataset: { owner: 'web-1', drawWindowId: String(0x8000_1234) },
  } as unknown as HTMLDivElement;
  assert.deepEqual(drawTargetFromTile(cameraTile), { ownerIdentity: 'web-1', windowId: 0x8000_1234 });
  assert.equal(cameraTile.dataset.windowId, undefined);

  const malformedCameraTile = { dataset: { owner: 'web-1' } } as unknown as HTMLDivElement;
  assert.equal(drawTargetFromTile(malformedCameraTile), null);
});

test('draw pen cursor embeds the identity color in an svg cursor', () => {
  const cursor = penCursor('#6e8bff');

  assert.match(cursor, /^url\("data:image\/svg\+xml,/);
  assert.match(cursor, /%236e8bff/);
  assert.match(cursor, /5 23, crosshair$/);
});

test('draw text builder emits one anchored single-line annotation', () => {
  const builder = createDrawMessageBuilder();
  const message = builder.text(
    { ownerIdentity: 'native-1', windowId: 42 },
    { x: 0.25, y: 0.5 },
    `hello\nworld${'x'.repeat(MAX_DRAW_TEXT_CHARS)}`
  );

  assert.equal(message.type, 'text');
  assert.deepEqual(message.points, [{ x: 0.25, y: 0.5 }]);
  assert.equal(message.text, `helloworld${'x'.repeat(MAX_DRAW_TEXT_CHARS - 10)}`);
});

function rootWith(tiles: Array<Pick<HTMLDivElement, 'dataset'>>) {
  return { querySelectorAll: () => tiles };
}

test('hasDrawableTarget treats share windows and camera tiles with drawWindowId as drawable', () => {
  assert.equal(hasDrawableTarget(rootWith([])), false);
  assert.equal(
    hasDrawableTarget(rootWith([{ dataset: { owner: 'native-1' } } as unknown as HTMLDivElement])),
    false
  );
  assert.equal(
    hasDrawableTarget(rootWith([{ dataset: { owner: 'native-1', windowId: '42' } } as unknown as HTMLDivElement])),
    true
  );
  assert.equal(
    hasDrawableTarget(
      rootWith([{ dataset: { owner: 'web-1', drawWindowId: String(0x8000_1234) } } as unknown as HTMLDivElement])
    ),
    true
  );
});

test('draw control copy disables and explains when nothing is drawable, and does not hide the button', () => {
  assert.deepEqual(drawControlCopy(false, false), {
    disabled: true,
    ariaLabel: DRAW_UNAVAILABLE_LABEL,
    title: DRAW_UNAVAILABLE_LABEL,
    tooltip: DRAW_UNAVAILABLE_LABEL,
  });
  assert.deepEqual(drawControlCopy(false, true), {
    disabled: true,
    ariaLabel: DRAW_UNAVAILABLE_LABEL,
    title: DRAW_UNAVAILABLE_LABEL,
    tooltip: DRAW_UNAVAILABLE_LABEL,
  });
  assert.equal(drawControlCopy(true, false).disabled, false);
  assert.equal(drawControlCopy(true, true).ariaLabel, 'Disable drawing');
  assert.equal(DRAW_UNAVAILABLE_LABEL, 'Nothing to draw on');
  assert.ok(
    DRAW_UNAVAILABLE_LABEL.length <= 22,
    'disabled tooltip must stay shorter than the 148px control tooltip'
  );
});
