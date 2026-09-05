<!--
  Dev-only deterministic test-pattern renderer (#256).

  This route is real, not a stand-in: it renders the same 960x600 reference
  canvas as web-harness/src/testPattern.ts. Keep constants in sync with
  docs/TEST_PATTERN_SPEC.md and the web-harness reference implementation.
-->
<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';

  interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
  }

  interface CornerCalibrationSquare extends Rect {
    color: string;
  }

  const TEST_PATTERN_WIDTH = 960;
  const TEST_PATTERN_HEIGHT = 600;
  const TEST_PATTERN_BACKGROUND = '#1b1033';
  const GRAY_CODE_BITS = 16;
  const GRAY_CODE_ZERO_COLOR = '#000000';
  const GRAY_CODE_ONE_COLOR = '#ffffff';
  const GRAY_CODE_STRIP: Rect = { x: 160, y: 88, w: 640, h: 30 };
  const GRAY_CODE_BLOCK_RECTS: Rect[] = Array.from({ length: GRAY_CODE_BITS }, (_, i) => ({
    x: GRAY_CODE_STRIP.x + i * 40,
    y: GRAY_CODE_STRIP.y,
    w: 40,
    h: GRAY_CODE_STRIP.h
  }));
  const CORNER_CALIBRATION_SQUARES: CornerCalibrationSquare[] = [
    { x: 16, y: 16, w: 24, h: 24, color: '#ff2d55' },
    { x: 920, y: 16, w: 24, h: 24, color: '#00ff88' },
    { x: 16, y: 560, w: 24, h: 24, color: '#2d7dff' },
    { x: 920, y: 560, w: 24, h: 24, color: '#ffd400' }
  ];
  const SHARPNESS_TARGET: Rect = { x: 352, y: 220, w: 256, h: 160 };
  const SHARPNESS_CHECKER_CELL_SIZE = 4;
  const DECORATIVE_CIRCLE_RADIUS = 32;
  const DECORATIVE_ORBIT_BOUNDS: Rect = { x: 96, y: 430, w: 768, h: 78 };
  const FRAME_COUNTER_MODULUS = 1 << GRAY_CODE_BITS;

  let canvas = $state<HTMLCanvasElement | null>(null);
  let timerHandle: ReturnType<typeof setInterval> | null = null;
  let frameCount = 0;
  // Repaint at 60fps via setInterval, NOT requestAnimationFrame: when this dev
  // window is backgrounded/occluded (as it is while the cockpit shares it),
  // the webview throttles rAF to ~1fps, so ScreenCaptureKit captures a nearly
  // static window and the receiver sees ~1fps. setInterval is not
  // background-throttled, so capture stays at full framerate. (#254 fps ceiling,
  // native side.)
  const TARGET_DRAW_FPS = 60;
  const LIVENESS_REPORT_EVERY_FRAMES = 6;
  let livenessCounter = 0;

  function frameCounterToGrayBlocks(counter: number): boolean[] {
    const wrapped = Math.trunc(counter) & (FRAME_COUNTER_MODULUS - 1);
    const gray = wrapped ^ (wrapped >> 1);
    return Array.from({ length: GRAY_CODE_BITS }, (_, i) => ((gray >> (GRAY_CODE_BITS - 1 - i)) & 1) === 1);
  }

  function drawGrayCodeStrip(ctx: CanvasRenderingContext2D) {
    const bits = frameCounterToGrayBlocks(frameCount);
    for (let i = 0; i < GRAY_CODE_BLOCK_RECTS.length; i += 1) {
      const rect = GRAY_CODE_BLOCK_RECTS[i];
      ctx.fillStyle = bits[i] ? GRAY_CODE_ONE_COLOR : GRAY_CODE_ZERO_COLOR;
      ctx.fillRect(rect.x, rect.y, rect.w, rect.h);
    }
  }

  function drawSharpnessTarget(ctx: CanvasRenderingContext2D) {
    for (let yy = 0; yy < SHARPNESS_TARGET.h; yy += SHARPNESS_CHECKER_CELL_SIZE) {
      for (let xx = 0; xx < SHARPNESS_TARGET.w; xx += SHARPNESS_CHECKER_CELL_SIZE) {
        const lit =
          ((xx / SHARPNESS_CHECKER_CELL_SIZE) + (yy / SHARPNESS_CHECKER_CELL_SIZE)) % 2 === 0;
        ctx.fillStyle = lit ? '#ffffff' : '#000000';
        ctx.fillRect(
          SHARPNESS_TARGET.x + xx,
          SHARPNESS_TARGET.y + yy,
          SHARPNESS_CHECKER_CELL_SIZE,
          SHARPNESS_CHECKER_CELL_SIZE
        );
      }
    }
  }

  function drawDecorativeCircle(ctx: CanvasRenderingContext2D) {
    const t = frameCount / 30;
    const cx = DECORATIVE_ORBIT_BOUNDS.x + (Math.sin(t * 0.7) * 0.5 + 0.5) * DECORATIVE_ORBIT_BOUNDS.w;
    const cy = DECORATIVE_ORBIT_BOUNDS.y + (Math.cos(t * 0.9) * 0.5 + 0.5) * DECORATIVE_ORBIT_BOUNDS.h;
    ctx.fillStyle = '#aa3bff';
    ctx.beginPath();
    ctx.arc(cx, cy, DECORATIVE_CIRCLE_RADIUS, 0, Math.PI * 2);
    ctx.fill();
    ctx.strokeStyle = '#00ff88';
    ctx.lineWidth = 6;
    ctx.stroke();
  }

  function drawFrame(ctx: CanvasRenderingContext2D) {
    frameCount = (frameCount + 1) & (FRAME_COUNTER_MODULUS - 1);
    ctx.fillStyle = TEST_PATTERN_BACKGROUND;
    ctx.fillRect(0, 0, TEST_PATTERN_WIDTH, TEST_PATTERN_HEIGHT);

    for (const square of CORNER_CALIBRATION_SQUARES) {
      ctx.fillStyle = square.color;
      ctx.fillRect(square.x, square.y, square.w, square.h);
    }
    drawGrayCodeStrip(ctx);
    drawSharpnessTarget(ctx);
    drawDecorativeCircle(ctx);

    ctx.fillStyle = '#ffffff';
    ctx.font = 'bold 30px system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.fillText('PETAL TEST PATTERN', TEST_PATTERN_WIDTH / 2, 60);
    ctx.font = 'bold 24px ui-monospace, monospace';
    ctx.fillStyle = '#00ff88';
    ctx.fillText(`frame ${frameCount}`, TEST_PATTERN_WIDTH / 2, 560);

    // Cockpit-only native readiness uses this synthetic counter to prove the
    // app-owned canvas is advancing. It carries no rendered pixels or page
    // content and is deliberately rate-limited below the draw cadence.
    if (frameCount % LIVENESS_REPORT_EVERY_FRAMES === 0) {
      livenessCounter += 1;
      void invoke('report_test_pattern_frame', { counter: livenessCounter }).catch(() => {});
    }
  }

  onMount(() => {
    const ctx = canvas?.getContext('2d');
    if (!canvas || !ctx) return;
    canvas.width = TEST_PATTERN_WIDTH;
    canvas.height = TEST_PATTERN_HEIGHT;
    drawFrame(ctx); // paint one immediately
    timerHandle = setInterval(() => drawFrame(ctx), Math.round(1000 / TARGET_DRAW_FPS));
    return () => {
      if (timerHandle !== null) clearInterval(timerHandle);
    };
  });
</script>

<svelte:head>
  <title>Petal Test Pattern</title>
</svelte:head>

<main>
  <canvas bind:this={canvas} width={TEST_PATTERN_WIDTH} height={TEST_PATTERN_HEIGHT}></canvas>
</main>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: #1b1033;
  }

  main {
    width: 100vw;
    height: 100vh;
    display: grid;
    place-items: center;
    background: #1b1033;
  }

  canvas {
    display: block;
    width: 960px;
    height: 600px;
  }
</style>
