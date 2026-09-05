export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface CornerCalibrationSquare extends Rect {
  name: 'topLeft' | 'topRight' | 'bottomLeft' | 'bottomRight';
  color: string;
}

export const TEST_PATTERN_WIDTH = 960;
export const TEST_PATTERN_HEIGHT = 600;
export const TEST_PATTERN_BACKGROUND = '#1b1033';
export const GRAY_CODE_BITS = 16;
export const GRAY_CODE_ZERO_COLOR = '#000000';
export const GRAY_CODE_ONE_COLOR = '#ffffff';
export const GRAY_CODE_STRIP: Rect = { x: 160, y: 88, w: 640, h: 30 };
export const GRAY_CODE_BLOCK_RECTS: Rect[] = Array.from({ length: GRAY_CODE_BITS }, (_, i) => ({
  x: GRAY_CODE_STRIP.x + i * 40,
  y: GRAY_CODE_STRIP.y,
  w: 40,
  h: GRAY_CODE_STRIP.h,
}));
export const CORNER_SQUARE_SIZE = 24;
export const CORNER_SQUARE_MARGIN = 16;
export const CORNER_CALIBRATION_SQUARES: CornerCalibrationSquare[] = [
  { name: 'topLeft', x: 16, y: 16, w: 24, h: 24, color: '#ff2d55' },
  { name: 'topRight', x: 920, y: 16, w: 24, h: 24, color: '#00ff88' },
  { name: 'bottomLeft', x: 16, y: 560, w: 24, h: 24, color: '#2d7dff' },
  { name: 'bottomRight', x: 920, y: 560, w: 24, h: 24, color: '#ffd400' },
];
export const SHARPNESS_TARGET: Rect = { x: 352, y: 220, w: 256, h: 160 };
export const SHARPNESS_CHECKER_CELL_SIZE = 4;
export const DECORATIVE_CIRCLE_RADIUS = 32;
export const DECORATIVE_ORBIT_BOUNDS: Rect = { x: 96, y: 430, w: 768, h: 78 };
export const DECORATIVE_CIRCLE_FILL = '#aa3bff';
export const DECORATIVE_CIRCLE_STROKE = '#00ff88';

const FRAME_COUNTER_MODULUS = 1 << GRAY_CODE_BITS;

export function frameCounterToGrayBlocks(counter: number): boolean[] {
  const wrapped = Math.trunc(counter) & (FRAME_COUNTER_MODULUS - 1);
  const gray = wrapped ^ (wrapped >> 1);
  return Array.from({ length: GRAY_CODE_BITS }, (_, i) => ((gray >> (GRAY_CODE_BITS - 1 - i)) & 1) === 1);
}

export function grayBlocksToFrameCounter(bits: boolean[]): number {
  if (bits.length !== GRAY_CODE_BITS) {
    throw new Error(`expected ${GRAY_CODE_BITS} Gray-code bits, got ${bits.length}`);
  }
  let gray = 0;
  for (const bit of bits) {
    gray = (gray << 1) | (bit ? 1 : 0);
  }
  let value = gray;
  for (let shift = 1; shift < GRAY_CODE_BITS; shift <<= 1) {
    value ^= value >> shift;
  }
  return value & (FRAME_COUNTER_MODULUS - 1);
}

function drawGrayCodeStrip(ctx: CanvasRenderingContext2D, frameCount: number) {
  const bits = frameCounterToGrayBlocks(frameCount);
  for (let i = 0; i < GRAY_CODE_BLOCK_RECTS.length; i += 1) {
    const rect = GRAY_CODE_BLOCK_RECTS[i];
    ctx.fillStyle = bits[i] ? GRAY_CODE_ONE_COLOR : GRAY_CODE_ZERO_COLOR;
    ctx.fillRect(rect.x, rect.y, rect.w, rect.h);
  }
}

function drawCornerCalibrationSquares(ctx: CanvasRenderingContext2D) {
  for (const square of CORNER_CALIBRATION_SQUARES) {
    ctx.fillStyle = square.color;
    ctx.fillRect(square.x, square.y, square.w, square.h);
  }
}

function drawSharpnessTarget(ctx: CanvasRenderingContext2D) {
  const { x, y, w, h } = SHARPNESS_TARGET;
  for (let yy = 0; yy < h; yy += SHARPNESS_CHECKER_CELL_SIZE) {
    for (let xx = 0; xx < w; xx += SHARPNESS_CHECKER_CELL_SIZE) {
      const lit = ((xx / SHARPNESS_CHECKER_CELL_SIZE) + (yy / SHARPNESS_CHECKER_CELL_SIZE)) % 2 === 0;
      ctx.fillStyle = lit ? '#ffffff' : '#000000';
      ctx.fillRect(x + xx, y + yy, SHARPNESS_CHECKER_CELL_SIZE, SHARPNESS_CHECKER_CELL_SIZE);
    }
  }
}

function drawDecorativeCircle(ctx: CanvasRenderingContext2D, frameCount: number) {
  const t = frameCount / 30;
  const cx = DECORATIVE_ORBIT_BOUNDS.x + (Math.sin(t * 0.7) * 0.5 + 0.5) * DECORATIVE_ORBIT_BOUNDS.w;
  const cy = DECORATIVE_ORBIT_BOUNDS.y + (Math.cos(t * 0.9) * 0.5 + 0.5) * DECORATIVE_ORBIT_BOUNDS.h;
  ctx.fillStyle = DECORATIVE_CIRCLE_FILL;
  ctx.beginPath();
  ctx.arc(cx, cy, DECORATIVE_CIRCLE_RADIUS, 0, Math.PI * 2);
  ctx.fill();
  ctx.strokeStyle = DECORATIVE_CIRCLE_STROKE;
  ctx.lineWidth = 6;
  ctx.stroke();
}

// Repaint cadence for the shared test pattern. `canvas.captureStream(30)` only
// emits a frame when the canvas is *repainted*, so the source must repaint at
// least as fast as the requested capture rate or delivered fps collapses to the
// paint rate. `requestAnimationFrame` is frame-rate/vsync throttled in headless
// Chrome (observed ~15fps) and is NOT covered by
// `--disable-background-timer-throttling`; a plain `setInterval` IS, so we drive
// repaints from a timer at a rate comfortably above the 30fps capture so every
// captured frame is fresh (#254 fps ceiling). 60fps draws cost little and give
// margin for timer jitter.
const TARGET_DRAW_FPS = 60;

/**
 * Allocate the capture canvas once, before the animation starts.
 *
 * Assigning either canvas dimension resets its backing store and 2D context.
 * Doing that inside the draw loop forces Chrome to repeatedly allocate a
 * 960x600 buffer while `captureStream()` is trying to read it, which can
 * collapse the synthetic sender's cadence even when the page stays visible.
 */
export function prepareTestPatternCanvas(canvas: HTMLCanvasElement): void {
  if (canvas.width !== TEST_PATTERN_WIDTH) canvas.width = TEST_PATTERN_WIDTH;
  if (canvas.height !== TEST_PATTERN_HEIGHT) canvas.height = TEST_PATTERN_HEIGHT;
}

export function createTestPattern(canvas: HTMLCanvasElement) {
  let canvasCtx: CanvasRenderingContext2D | null = null;
  let timerHandle: ReturnType<typeof setInterval> | null = null;
  let frameCount = 0;

  function drawFrame() {
    if (!canvasCtx) return;
    frameCount = (frameCount + 1) & (FRAME_COUNTER_MODULUS - 1);

    canvasCtx.fillStyle = TEST_PATTERN_BACKGROUND;
    canvasCtx.fillRect(0, 0, TEST_PATTERN_WIDTH, TEST_PATTERN_HEIGHT);

    drawCornerCalibrationSquares(canvasCtx);
    drawGrayCodeStrip(canvasCtx, frameCount);
    drawSharpnessTarget(canvasCtx);
    drawDecorativeCircle(canvasCtx, frameCount);

    canvasCtx.fillStyle = '#ffffff';
    canvasCtx.font = 'bold 30px system-ui, sans-serif';
    canvasCtx.textAlign = 'center';
    canvasCtx.fillText('PETAL TEST PATTERN', TEST_PATTERN_WIDTH / 2, 60);

    canvasCtx.font = 'bold 24px ui-monospace, monospace';
    canvasCtx.fillStyle = '#00ff88';
    canvasCtx.fillText(`frame ${frameCount}`, TEST_PATTERN_WIDTH / 2, 560);
  }

  function startCanvasAnimation() {
    prepareTestPatternCanvas(canvas);
    canvasCtx = canvas.getContext('2d');
    if (timerHandle === null) {
      drawFrame(); // paint one immediately so captureStream has content at t=0
      timerHandle = setInterval(drawFrame, Math.round(1000 / TARGET_DRAW_FPS));
    }
  }

  // Exposed for the test-cockpit self-check (#254): "is this headless
  // renderer's repaint loop actually advancing" is measured by sampling this
  // counter's delta over a short window, before anything joins a room -- a
  // frozen/throttled headless Chrome is an INFRA-FAIL, not a Petal
  // regression.
  function getFrameCount(): number {
    return frameCount;
  }

  return { startCanvasAnimation, getFrameCount };
}
