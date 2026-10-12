// The native gallery at the window shapes people actually drag it to, in
// Chromium, through the REAL meeting composition (tests/fixtures/gallery-lab.js
// mounts $lib/dev/GalleryLabStage.svelte: MeetingChrome -> Gallery ->
// ParticipantTile, synthetic cameras). The CI slice of the layout lab; the
// full matrix with screenshots is scripts/verify-native-gallery-matrix.mjs.
//
// Every cell must pass the lab's own rules (galleryLabMeasure.ts
// judgeGalleryLab: nothing scrolls, overlaps or clips, Mic/Camera/Share/Leave
// on screen, the strip centred, your thumbnail never smaller, faces big
// enough, the window going to faces), and the shapes this lab exists for
// must come out the way they were asked for:
//  - narrow and tall: a vertical column of faces; in spotlight, the hero
//    over a centred column of smaller thumbnails;
//  - short and wide: one row of faces, shorter tiles rather than two rows,
//    with the controls in a rail at the right edge and the top bar floating
//    over the tiles only while the pointer is over the window;
//  - spotlight thumbnails centred, never hugging the left edge.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium } from 'playwright';
import { build } from 'vite';
import type { GalleryLabReading } from '../src/lib/dev/galleryLabMeasure.ts';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

interface Cell {
  w: number;
  h: number;
  n: number;
  mode: 'grid' | 'spotlight';
  extra?: string;
}

const query = (cell: Cell) => `w=${cell.w}&h=${cell.h}&n=${cell.n}&mode=${cell.mode}&plugins=1${cell.extra ?? ''}`;
const distinct = (values: number[]) => new Set(values.map((value) => Math.round(value))).size;

test('the native gallery lays out every window shape: a column, a bar, centred spotlight strips', { timeout: 180_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-gallery-lab-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;
  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: resolve(fileURLToPath(new URL('./src/lib', desktopRoot))),
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./gallery-lab.html', fixtureRoot)) } }
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu', '--autoplay-policy=no-user-gesture-required'] });
    const page = await browser.newPage({ viewport: { width: 841, height: 561 } });
    const pageErrors: string[] = [];
    page.on('pageerror', (error: Error) => pageErrors.push(error.message));
    await page.goto(`${pathToFileURL(join(buildDir, 'gallery-lab.html')).href}#w=840&h=560&n=4&mode=grid`);
    await page.waitForFunction(() => document.body.dataset.ready === 'true', undefined, { timeout: 30_000 });

    // The viewport IS the window, as in the app, plus one spare pixel to
    // park the pointer outside it (the resting state).
    const show = async (cell: Cell): Promise<GalleryLabReading> => {
      await page.setViewportSize({ width: cell.w + 1, height: cell.h + 1 });
      await page.mouse.move(cell.w, cell.h);
      const { reading, problems } = (await page.evaluate(`window.__galleryLab.set(${JSON.stringify(query(cell))})`)) as {
        reading: GalleryLabReading;
        problems: string[];
      };
      assert.deepEqual(problems, [], `${query(cell)}: ${problems.join('; ')}`);
      return reading;
    };

    // The default window and the big ones keep the regular chrome.
    for (const cell of [
      { w: 840, h: 560, n: 4, mode: 'grid' },
      { w: 840, h: 560, n: 6, mode: 'spotlight' },
      { w: 1920, h: 1080, n: 9, mode: 'grid' },
      { w: 1280, h: 800, n: 5, mode: 'spotlight', extra: '&aspect=mixed' },
      { w: 840, h: 560, n: 5, mode: 'grid', extra: '&chat=1' }
    ] as Cell[]) {
      const reading = await show(cell);
      assert.ok(!reading.galleryClasses.includes('rail-controls'), `${query(cell)}: no rail at this size`);
    }

    // Narrow and tall: a column of faces.
    const column = await show({ w: 300, h: 900, n: 4, mode: 'grid' });
    assert.equal(distinct(column.tiles.map((t) => t.rect.left)), 1, `one column: ${JSON.stringify(column.tiles.map((t) => t.rect))}`);
    assert.ok(column.smallestTile.width >= 250, `the column is as wide as the window allows (${column.smallestTile.width}px)`);
    // ...and in spotlight, the hero over a centred column of smaller thumbnails.
    const columnSpotlight = await show({ w: 300, h: 900, n: 6, mode: 'spotlight' });
    const hero = columnSpotlight.tiles.find((t) => t.role === 'hero')!;
    const thumbs = columnSpotlight.tiles.filter((t) => t.role === 'thumbnail');
    assert.equal(columnSpotlight.stripPlacement, 'below');
    assert.ok(thumbs.every((t) => t.rect.top >= hero.rect.top + hero.rect.height - 1), 'thumbnails under the hero');
    assert.ok(thumbs.every((t) => t.rect.width < hero.rect.width), 'thumbnails smaller than the hero');
    assert.equal(distinct(thumbs.map((t) => t.rect.left)), 1, 'thumbnails in one column');
    // Inside the band where the control row once flipped its labels on and
    // off every two frames (judgeGalleryLab samples 20 idle frames).
    await show({ w: 470, h: 800, n: 4, mode: 'grid' });
    // A short column -- narrow, and too short for a top bar row: the top bar
    // floats there too, and the smallest narrow gallery before the pill.
    const shortColumn = await show({ w: 300, h: 450, n: 4, mode: 'grid' });
    assert.ok(shortColumn.galleryClasses.includes('floating-topbar'), `${shortColumn.galleryClasses}`);
    await show({ w: 240, h: 360, n: 3, mode: 'grid' });
    // The narrowest the gallery allows still lays out (GALLERY_MIN).
    await show({ w: 240, h: 700, n: 5, mode: 'grid' });
    await show({ w: 240, h: 700, n: 5, mode: 'spotlight' });

    // Short and wide: one row of shorter faces, controls in the rail.
    const bar = await show({ w: 1100, h: 170, n: 9, mode: 'grid' });
    assert.ok(bar.galleryClasses.includes('rail-controls'), `a bar stands its controls in a rail: ${bar.galleryClasses}`);
    assert.equal(distinct(bar.tiles.map((t) => t.rect.top)), 1, `nine people in a bar are one row: ${JSON.stringify(bar.tiles.map((t) => t.rect))}`);
    assert.ok(bar.controlbar && bar.controlbar.top <= 0.5 && bar.controlbar.height >= 170 - 0.5, 'the rail runs the full height');
    assert.ok(bar.controlbar!.left + bar.controlbar!.width >= 1100 - 1, 'the rail is at the right edge');
    // The top bar floats: hidden at rest, shown while the pointer is over the window.
    const topbarOpacity = () => page.evaluate(`Number(getComputedStyle(document.querySelector('.gallery .topbar')).opacity)`);
    await page.waitForFunction(`Number(getComputedStyle(document.querySelector('.gallery .topbar')).opacity) === 0`, undefined, { timeout: 2_000 });
    await page.mouse.move(500, 100);
    await page.waitForFunction(`Number(getComputedStyle(document.querySelector('.gallery .topbar')).opacity) === 1`, undefined, { timeout: 2_000 });
    assert.equal(await topbarOpacity(), 1);
    await page.mouse.move(1100, 170);
    // ...and even shown, it takes clicks only on its buttons: where it lies
    // over a tile (two people in a 1400x220 bar fill the height), the tile is
    // what a click there reaches.
    await show({ w: 1400, h: 220, n: 2, mode: 'grid' });
    await page.mouse.move(700, 120);
    await page.waitForFunction(`Number(getComputedStyle(document.querySelector('.gallery .topbar')).opacity) === 1`, undefined, { timeout: 2_000 });
    const underBar = (await page.evaluate(`(() => {
      const bar = document.querySelector('.gallery .topbar').getBoundingClientRect();
      const tile = [...document.querySelectorAll('.tile-wrap')].map((el) => el.getBoundingClientRect()).find((r) => r.top < bar.bottom - 8);
      if (!tile) return 'no tile under the bar';
      const hit = document.elementFromPoint(tile.left + tile.width / 2, (Math.max(tile.top, bar.top) + bar.bottom) / 2);
      return hit?.closest('.tile-wrap') ? 'tile' : String(hit?.className);
    })()`)) as string;
    assert.equal(underBar, 'tile', 'the floating top bar passes clicks to the tile beneath it');
    await page.mouse.move(1400, 220);

    const barSpotlight = await show({ w: 1400, h: 220, n: 6, mode: 'spotlight' });
    assert.equal(barSpotlight.stripPlacement, 'side');
    const barThumbs = barSpotlight.tiles.filter((t) => t.role === 'thumbnail');
    assert.equal(distinct(barThumbs.map((t) => t.rect.top)), 1, 'a bar spotlight puts every thumbnail in one row beside the hero, not a scrolling column');
    // The shortest the gallery allows (GALLERY_MIN), and chat open in a bar.
    await show({ w: 900, h: 160, n: 6, mode: 'grid' });
    await show({ w: 1400, h: 220, n: 5, mode: 'grid', extra: '&chat=1' });

    // Spotlight thumbnails under the hero are centred, not left aligned
    // (judgeGalleryLab checks the offset; this pins that the case ran).
    const tall = await show({ w: 600, h: 1000, n: 5, mode: 'spotlight' });
    assert.equal(tall.stripPlacement, 'below');
    assert.ok(tall.stripCentreOffset !== null && Math.abs(tall.stripCentreOffset) <= 2, `centred: ${tall.stripCentreOffset}`);

    assert.deepEqual(pageErrors, []);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
