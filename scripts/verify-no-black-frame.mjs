#!/usr/bin/env node
/**
 * #627 / CLAUDE.md "Never show a black frame": prove by SAMPLED RENDERED PIXELS
 * that a share tile holds its last frame across a disruption instead of going
 * black.
 *
 * Why pixels and not events: "held the last frame" and "went black quietly"
 * emit exactly the same events. An event-level assertion cannot tell them
 * apart, so it is not evidence. This drives a real Chromium compositor, forces
 * the real gap (an emptied `srcObject`, which is what a republish leaves
 * behind), and screenshots the tile.
 *
 * It runs BOTH directions, which is the whole point:
 *   - with the hold attached    -> the tile must STAY BRIGHT across the gap
 *   - with the hold NOT attached -> the tile must GO BLACK across the gap
 * Without the second run a passing first run would be worthless: it could not
 * distinguish a working hold from a gap that never actually happened.
 *
 * Usage: node scripts/verify-no-black-frame.mjs
 */

import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '..');
const require = createRequire(import.meta.url);

let chromium;
try {
  const playwrightModule =
    process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium } = require(playwrightModule));
} catch (error) {
  console.error(
    `Requires Playwright. Install apps/desktop dependencies or set PETAL_PLAYWRIGHT_MODULE. ${
      error instanceof Error ? error.message : String(error)
    }`
  );
  process.exit(2);
}

// Bundle the REAL module rather than reimplementing its logic in the fixture.
const workDir = mkdtempSync(join(tmpdir(), 'flicker627-'));
const bundlePath = join(workDir, 'holdLastFrame.js');
const esbuildBin = resolve(repoRoot, 'web-harness/node_modules/esbuild/bin/esbuild');
execFileSync(
  esbuildBin,
  [
    resolve(repoRoot, 'web-harness/src/holdLastFrame.ts'),
    '--bundle',
    '--format=iife',
    '--global-name=PetalHold',
    `--outfile=${bundlePath}`,
  ],
  { stdio: 'pipe' }
);
const holdBundle = readFileSync(bundlePath, 'utf8');

// The REAL production stylesheet, so `.tile video { background: #000 }` and the
// hold-canvas rules are exercised exactly as shipped.
const productionCss = readFileSync(resolve(repoRoot, 'web-harness/src/style.css'), 'utf8');

/** A sampled frame counts as black when nearly every pixel is near-zero. */
const BLACK_LUMA = 12;
const GAP_SAMPLE_MS = 600;

const page_html = `
<div id="tiles">
  <div class="tile share-tile" id="probe-tile" style="position:relative;width:480px;height:320px;">
    <video class="share-video" id="probe-video" autoplay playsinline muted></video>
    <div class="name-chip">probe</div>
  </div>
</div>`;

const browser = await chromium.launch({
  headless: true,
  args: ['--autoplay-policy=no-user-gesture-required'],
  ...(process.env.PETAL_CHROME_BIN ? { executablePath: process.env.PETAL_CHROME_BIN } : {}),
});

/**
 * @param {{ withHold: boolean, shippedCss: boolean }} options
 *   withHold   - attach the hold-last-frame mechanism
 *   shippedCss - restore the pre-fix `background: #000` on the share video, i.e.
 *                reproduce what shipped 0.8.0 actually rendered
 * @returns {Promise<{ beforeLuma: number, samples: number[], holdingReason: string|null }>}
 */
async function runGapTrial({ withHold, shippedCss }) {
  const page = await browser.newPage({ viewport: { width: 640, height: 480 } });
  try {
    await page.setContent(`<!doctype html><html><body style="margin:0;background:#123">${page_html}</body></html>`);
    await page.addStyleTag({ content: productionCss });
    if (shippedCss) {
      // Undo the fix's transparent background so this trial renders exactly
      // what 0.8.0 rendered. Without this the "control" would silently inherit
      // half the fix and could never go black.
      await page.addStyleTag({ content: '.tile video.share-video { background: #000 !important; }' });
    }
    await page.addScriptTag({ content: holdBundle });

    // A bright, unmistakable source. If the tile is ever near-black the source
    // is not what we are looking at.
    await page.evaluate(async ({ withHold: attachHold }) => {
      const video = document.getElementById('probe-video');
      const tile = document.getElementById('probe-tile');
      const canvas = document.createElement('canvas');
      canvas.width = 320;
      canvas.height = 200;
      const context = canvas.getContext('2d');
      let tick = 0;
      const paint = () => {
        tick += 1;
        context.fillStyle = '#ffffff';
        context.fillRect(0, 0, canvas.width, canvas.height);
        context.fillStyle = '#ff2d55';
        context.fillRect((tick * 7) % canvas.width, 0, 40, canvas.height);
      };
      paint();
      window.__paintTimer = setInterval(paint, 33);
      const stream = canvas.captureStream(30);
      window.__sourceStream = stream;
      video.srcObject = stream;
      await video.play().catch(() => {});
      // Wait for a genuinely decoded frame before doing anything else.
      await new Promise((done) => {
        const check = () => (video.videoWidth > 0 && video.readyState >= 2 ? done() : requestAnimationFrame(check));
        check();
      });
      if (attachHold) {
        window.__holdRegistry = new WeakMap();
        window.__hold = window.PetalHold.attachHoldLastFrame(tile, video, window.__holdRegistry);
        if (!window.__hold) throw new Error('attachHoldLastFrame returned null in the fixture');
      }
    }, { withHold });

    // Let the hold capture at least one copy (HOLD_REFRESH_MS is 200ms).
    await page.waitForTimeout(500);

    const beforeLuma = await sampleLuma(page);

    // FORCE THE GAP. An emptied srcObject is precisely the state a republish
    // leaves the element in: alive, laid out, and with no frame to present.
    await page.evaluate(() => {
      const video = document.getElementById('probe-video');
      clearInterval(window.__paintTimer);
      window.__sourceStream.getTracks().forEach((track) => track.stop());
      video.srcObject = new MediaStream();
      window.__hold?.noteGap('source-swap');
    });

    const samples = [];
    const deadline = Date.now() + GAP_SAMPLE_MS;
    while (Date.now() < deadline) {
      samples.push(await sampleLuma(page));
    }
    const holdingReason = await page.evaluate(
      () => document.getElementById('probe-tile').dataset.shareHoldingFrame ?? null
    );
    return { beforeLuma, samples, holdingReason };
  } finally {
    await page.close();
  }
}

/**
 * Mean luminance of the middle of the rendered VIDEO area, straight off the
 * compositor. Deliberately the video's own letterboxed content box and not the
 * whole tile: the tile also contains a name chip and a border, whose brightness
 * would dilute the measurement and let a genuinely black video still read as
 * "not black".
 */
async function sampleLuma(page) {
  const box = await page.evaluate(() => {
    const video = document.getElementById('probe-video');
    const rect = video.getBoundingClientRect();
    // The video letterboxes with object-fit: contain, so shrink to the middle
    // half in each axis -- guaranteed inside the painted media on any aspect.
    return {
      x: Math.round(rect.left + rect.width * 0.25),
      y: Math.round(rect.top + rect.height * 0.25),
      width: Math.max(8, Math.round(rect.width * 0.5)),
      height: Math.max(8, Math.round(rect.height * 0.5)),
    };
  });
  const shot = await page.screenshot({ clip: box, type: 'png' });
  return await page.evaluate(async (bytes) => {
    const blob = new Blob([new Uint8Array(bytes)], { type: 'image/png' });
    const bitmap = await createImageBitmap(blob);
    const canvas = document.createElement('canvas');
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    const context = canvas.getContext('2d');
    context.drawImage(bitmap, 0, 0);
    const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
    let total = 0;
    for (let i = 0; i < data.length; i += 4) {
      total += 0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2];
    }
    return total / (data.length / 4);
  }, Array.from(shot));
}

const failures = [];
function check(condition, message) {
  if (condition) console.log(`  ok   ${message}`);
  else {
    console.log(`  FAIL ${message}`);
    failures.push(message);
  }
}

try {
  console.log('#627 no-black-frame, sampled rendered pixels\n');

  // ---- Trial 1: reproduce the bug as it shipped -------------------------
  console.log('trial 1: BASELINE -- shipped 0.8.0 (background:#000, no hold)');
  const shipped = await runGapTrial({ withHold: false, shippedCss: true });
  const shippedMin = Math.min(...shipped.samples);
  console.log(`  before=${shipped.beforeLuma.toFixed(1)} during: min=${shippedMin.toFixed(1)} n=${shipped.samples.length}`);
  check(shipped.beforeLuma > BLACK_LUMA * 4, `source renders bright before the gap (${shipped.beforeLuma.toFixed(1)})`);
  check(
    shippedMin <= BLACK_LUMA,
    `shipped 0.8.0 DOES go black across the gap (min luma ${shippedMin.toFixed(1)} <= ${BLACK_LUMA}) ` +
      '-- this is the reproduction; if it fails the gap is not being forced and nothing below proves anything'
  );

  // ---- Trial 2: the fix -------------------------------------------------
  console.log('\ntrial 2: FIXED -- transparent background + hold-last-frame');
  const fixed = await runGapTrial({ withHold: true, shippedCss: false });
  const fixedMin = Math.min(...fixed.samples);
  console.log(
    `  before=${fixed.beforeLuma.toFixed(1)} during: min=${fixedMin.toFixed(1)} ` +
      `n=${fixed.samples.length} holdingReason=${fixed.holdingReason}`
  );
  check(fixedMin > BLACK_LUMA, `tile never goes black during the gap (min luma ${fixedMin.toFixed(1)} > ${BLACK_LUMA})`);
  check(
    Math.abs(fixedMin - fixed.beforeLuma) < 6,
    `the HELD FRAME is what is shown, not a placeholder: during=${fixedMin.toFixed(1)} ` +
      `stays within 6 luma of before=${fixed.beforeLuma.toFixed(1)}`
  );
  check(fixed.holdingReason !== null, 'tile reports it is holding a frame');

  // ---- Trial 3: which half of the fix does what -------------------------
  // The transparent background alone stops BLACK but shows the tile's own
  // background, not the frame. This pins that distinction so a future change
  // cannot quietly drop the canvas and still pass on the CSS alone.
  console.log('\ntrial 3: DECOMPOSITION -- transparent background, hold NOT attached');
  const cssOnly = await runGapTrial({ withHold: false, shippedCss: false });
  const cssOnlyMin = Math.min(...cssOnly.samples);
  console.log(`  before=${cssOnly.beforeLuma.toFixed(1)} during: min=${cssOnlyMin.toFixed(1)} n=${cssOnly.samples.length}`);
  check(
    cssOnlyMin > BLACK_LUMA,
    `transparent background alone already avoids pure black (min luma ${cssOnlyMin.toFixed(1)})`
  );
  check(
    cssOnlyMin < cssOnly.beforeLuma - 20,
    `but it does NOT preserve the frame (during=${cssOnlyMin.toFixed(1)} well below before=${cssOnly.beforeLuma.toFixed(1)}) ` +
      '-- so the hold canvas is load-bearing, not decorative'
  );

  console.log('');
  if (failures.length > 0) {
    console.error(`no-black-frame verification FAILED (${failures.length}):`);
    for (const failure of failures) console.error(`  - ${failure}`);
    process.exit(1);
  }
  console.log('no-black-frame verification PASSED (pixels sampled; bug reproduced AND fixed in the same run)');
} finally {
  await browser.close();
}
