// #248: a keyed-list FLIP (Svelte `animate:`) that interrupts another one must
// start from the box the tile was visibly showing. Svelte measures the old
// rect with getBoundingClientRect() -- the running FLIP's transform, but not
// its clip -- and cancels that FLIP before calling the animate function, so
// only a real Svelte + Chromium run shows whether the handover is seamless.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium } from 'playwright';
import { build } from 'vite';

import { visibleFlipRect, type FlipRect } from '@petal/shared/logic/tileFlip';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

interface TileReading {
  painted: FlipRect;
  layoutWidth: number;
  clipPath: string;
  animations: number;
}

test('a join during a shape-changing keyed-list FLIP carries on from the visible box', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-keyed-tile-flip-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot))),
        },
      },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./keyed-tile-flip.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });
    const context = await browser.newContext({ viewport: { width: 900, height: 700 }, reducedMotion: 'no-preference' });
    // The fixture is self-contained: nothing may leave the machine.
    await context.route('**/*', (route) =>
      /^(file:|https?:\/\/(127\.0\.0\.1|localhost)[:/])/.test(route.request().url()) ? route.continue() : route.abort()
    );
    const page = await context.newPage();
    await page.goto(pathToFileURL(join(buildDir, 'keyed-tile-flip.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');

    // First join: 16:9 -> square, a clipped FLIP of 8 s.
    await page.evaluate(() => (window as any).__keyedTileFlip.join());
    await page.waitForFunction(() => document.querySelector('[data-id="a"]')!.getAnimations().length > 0);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 300));

    // Second join mid-flight: read the tile, join, and read it again before
    // any frame can pass -- Svelte applies the new FLIP's first frame in a
    // microtask, so both readings are of the same instant.
    // (A string, not a function: tsx would wrap a named helper in `__name`,
    // which does not exist in the page.)
    const { before, after } = (await page.evaluate(`(async () => {
      const tile = document.querySelector('[data-id="a"]');
      const read = () => {
        const { left, top, width, height } = tile.getBoundingClientRect();
        return {
          painted: { left, top, width, height },
          layoutWidth: tile.offsetWidth,
          clipPath: getComputedStyle(tile).clipPath,
          animations: tile.getAnimations().length,
        };
      };
      const before = read();
      window.__keyedTileFlip.join();
      for (let i = 0; i < 8; i += 1) await Promise.resolve();
      return { before, after: read() };
    })()`)) as { before: TileReading; after: TileReading };

    const visibleBefore = visibleFlipRect(before.painted, before.layoutWidth, before.clipPath);
    assert.ok(
      before.painted.height - visibleBefore.height > 20,
      `the second join must land mid-flight, while the tile is clipped: ${JSON.stringify(before)}`
    );
    assert.ok(after.animations > 0, 'the second join animates');
    assert.equal(after.layoutWidth, 300, 'the tile is laid out at its new shape');
    const visibleAfter = visibleFlipRect(after.painted, after.layoutWidth, after.clipPath);
    for (const key of ['left', 'top', 'width', 'height'] as const) {
      assert.ok(
        Math.abs(visibleAfter[key] - visibleBefore[key]) < 1,
        `${key} jumped: ${visibleBefore[key]} -> ${visibleAfter[key]} (before ${JSON.stringify(before)}, after ${JSON.stringify(after)})`
      );
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
