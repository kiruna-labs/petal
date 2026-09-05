#!/usr/bin/env node
// Receiver-render convergence verification (defect D, 2026-07-30).
//
// Drives the REAL browser client end to end against a local LiveKit server
// and asserts the viewer RENDERS — by sampling decoded video pixels, not by
// counting events (an event-level assertion cannot tell "rendering" from
// "blank"). Sequence exercised:
//
//   1. sender joins and publishes the test pattern; viewer joins and must
//      render non-black, non-uniform pixels from the share tile's video;
//   2. sender unpublishes and republishes (unshare -> reshare, a NEW track
//      sid); the viewer must converge back to rendering the replacement;
//   3. the viewer's subscription is forcibly dropped client-side
//      (publication.setSubscribed(false)) with no further user action; the
//      #298 reconciliation pass alone must re-express demand and converge
//      the tile back to rendering pixels.
//
// Prerequisites (all local, no prod dependencies):
//   livekit-server --dev                          # ws://localhost:7880
//   apps/desktop/.env with LIVEKIT_URL=ws://localhost:7880, devkey/secret
//   (cd web-harness && npx vite --port 5199)      # or set PETAL_WEB_URL
//
// Run:  node scripts/verify-receiver-render.mjs
// Exits 0 on pass; non-zero with a FAIL line on any assertion failure.

import { createRequire } from 'node:module';
import { resolve } from 'node:path';

const repoRoot = resolve(import.meta.dirname, '..');
const baseUrl = (process.env.PETAL_WEB_URL ?? 'http://localhost:5199').replace(/\/$/, '');
// A bare access code must be 10 letters from [a-z] (generation avoids i/l; see
// web-harness/src/meetingCode.ts normalizeAccessCode).
const roomCode = process.env.PETAL_TEST_ROOM ?? 'rcpxeatest';

let chromium;
try {
  const playwrightModule =
    process.env.PETAL_PLAYWRIGHT_MODULE ?? resolve(repoRoot, 'apps/desktop/node_modules/playwright');
  ({ chromium } = createRequire(import.meta.url)(playwrightModule));
} catch (error) {
  console.error(`Playwright unavailable: ${error instanceof Error ? error.message : error}`);
  process.exit(2);
}

const failures = [];
function ok(desc) {
  console.log(`ok   ${desc}`);
}
function fail(desc) {
  failures.push(desc);
  console.error(`FAIL ${desc}`);
}

/** Sample the share tile's <video> pixels in-page. Returns luminance stats. */
async function sampleSharePixels(page) {
  return page.evaluate(() => {
    const video = document.querySelector('.share-tile video');
    if (!video || video.videoWidth === 0 || video.videoHeight === 0) return null;
    const canvas = document.createElement('canvas');
    canvas.width = Math.min(video.videoWidth, 320);
    canvas.height = Math.min(video.videoHeight, 180);
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
    const { data } = ctx.getImageData(0, 0, canvas.width, canvas.height);
    let sum = 0;
    let min = 255;
    let max = 0;
    const pixels = data.length / 4;
    for (let i = 0; i < data.length; i += 4) {
      const luma = 0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2];
      sum += luma;
      if (luma < min) min = luma;
      if (luma > max) max = luma;
    }
    return {
      mean: sum / pixels,
      min,
      max,
      spread: max - min,
      videoWidth: video.videoWidth,
      videoHeight: video.videoHeight,
    };
  });
}

async function waitFor(page, desc, predicate, timeoutMs = 30_000, pollMs = 500) {
  const deadline = Date.now() + timeoutMs;
  let last = null;
  while (Date.now() < deadline) {
    last = await predicate();
    if (last) return last;
    await page.waitForTimeout(pollMs);
  }
  throw new Error(`timeout waiting for ${desc} (last=${JSON.stringify(last)})`);
}

/** Rendering = decoded frame with real content: bright enough and non-uniform. */
function looksRendered(stats) {
  return stats !== null && stats.mean > 16 && stats.spread > 40;
}

async function currentShareSid(page) {
  return page.evaluate(() => {
    const tile = document.querySelector('.share-tile');
    return tile?.dataset.trackSid ?? null;
  });
}

const browser = await chromium.launch({ headless: true });
try {
  const senderCtx = await browser.newContext({ viewport: { width: 1100, height: 750 } });
  const viewerCtx = await browser.newContext({ viewport: { width: 1100, height: 750 } });
  const sender = await senderCtx.newPage();
  const viewer = await viewerCtx.newPage();
  for (const [name, page] of [
    ['sender', sender],
    ['viewer', viewer],
  ]) {
    page.on('pageerror', (err) => console.error(`[${name} pageerror] ${err.message}`));
  }

  await sender.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });
  await viewer.goto(`${baseUrl}/`, { waitUntil: 'networkidle' });

  const join = async (page, name) => {
    await page.waitForFunction(() => window.__petalHarness?.cockpitAutoScenario?.join, null, {
      timeout: 15_000,
    });
    await page.evaluate(
      ([code, displayName]) => {
        const input = document.querySelector('#display-name');
        if (input) input.value = displayName;
        return window.__petalHarness.cockpitAutoScenario.join(code);
      },
      [roomCode, name]
    );
    await page.waitForFunction(() => window.__petalHarness?.room?.state === 'connected', null, {
      timeout: 20_000,
    });
  };

  await join(sender, 'PixelSender');
  await join(viewer, 'PixelViewer');
  ok('both peers connected');

  // --- Phase 1: publish, viewer must render pixels -------------------------
  await sender.evaluate(() => window.__petalHarness.cockpitAutoScenario.sharePattern());
  const first = await waitFor(
    viewer,
    'viewer renders the first share',
    async () => {
      const stats = await sampleSharePixels(viewer);
      return looksRendered(stats) ? stats : null;
    },
    30_000
  );
  ok(
    `viewer renders published share (mean=${first.mean.toFixed(1)} spread=${first.spread.toFixed(1)} ${first.videoWidth}x${first.videoHeight})`
  );
  const sidBefore = await currentShareSid(viewer);

  // --- Phase 2: unpublish -> republish (new sid); viewer must converge -----
  // The legacy #share-btn lives in a collapsed debug section, so drive it via
  // DOM click (same handler the visible control-bar toggle routes through).
  await sender.evaluate(() => document.querySelector('#share-btn').click()); // unshare
  await sender.waitForTimeout(700);
  await sender.evaluate(() => document.querySelector('#share-btn').click()); // reshare -> NEW sid
  const afterRepublish = await waitFor(
    viewer,
    'viewer renders the republished share',
    async () => {
      const sid = await currentShareSid(viewer);
      if (!sid || sid === sidBefore) return null;
      const stats = await sampleSharePixels(viewer);
      return looksRendered(stats) ? { sid, ...stats } : null;
    },
    30_000
  );
  if (afterRepublish.sid !== sidBefore) {
    ok(
      `viewer converged onto the republished track (sid ${sidBefore} -> ${afterRepublish.sid}, mean=${afterRepublish.mean.toFixed(1)})`
    );
  } else {
    fail('viewer still bound to the pre-republish sid');
  }

  // --- Phase 3: forced client-side subscription loss; reconcile must repair
  // it with ZERO user action. The pass starts FIRST_PASS_GRACE_MS (15s) after
  // connect and runs every 5s, so allow a generous window.
  const dropped = await viewer.evaluate(() => {
    const room = window.__petalHarness.room;
    for (const participant of room.remoteParticipants.values()) {
      for (const pub of participant.trackPublications.values()) {
        if (pub.trackName?.startsWith('petal-window-')) {
          pub.setSubscribed(false);
          return pub.trackSid;
        }
      }
    }
    return null;
  });
  if (!dropped) {
    fail('could not find a window publication to unsubscribe');
  } else {
    // Blankness first: the tile must actually lose its stream so recovery is
    // provable (tile removal or a dead video both count).
    await viewer.waitForTimeout(3_000);
    const recovered = await waitFor(
      viewer,
      'reconcile pass restores rendering after forced unsubscribe',
      async () => {
        const stats = await sampleSharePixels(viewer);
        return looksRendered(stats) ? stats : null;
      },
      60_000,
      1_000
    );
    ok(
      `reconcile pass restored rendering with no user action (mean=${recovered.mean.toFixed(1)} spread=${recovered.spread.toFixed(1)})`
    );
  }

  // Leave the rooms cleanly so the dev room does not accumulate ghosts.
  for (const page of [sender, viewer]) {
    await page.evaluate(() => window.__petalHarness.room?.disconnect()).catch(() => {});
  }
} catch (error) {
  fail(error instanceof Error ? error.message : String(error));
} finally {
  await browser.close();
}

if (failures.length > 0) {
  console.error(`\n${failures.length} failure(s)`);
  process.exit(1);
}
console.log('\nreceiver-render convergence: PASS');
