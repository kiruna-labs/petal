#!/usr/bin/env node
// Native gallery layout matrix: the desktop app's counterpart of
// scripts/verify-meeting-layout-matrix.mjs (#239, the web client).
//
// Renders the REAL meeting composition (MeetingChrome -> Gallery ->
// ParticipantTile, via apps/desktop/src/lib/dev/GalleryLabStage.svelte) in
// Chromium with synthetic cameras, across window shapes (default, laptop,
// full screen, a narrow column of faces, a short bar of faces, ...), people
// counts, grid and spotlight, chat open, cameras off and mixed camera shapes.
// The page viewport IS the window, as in the app, so the gallery's media
// queries see what they see in Tauri. For every cell it saves a screenshot
// and records, from rendered geometry (src/lib/dev/galleryLabMeasure.ts):
//   - the share of the window the tiles, and their camera pictures, cover;
//     how much of the window the chrome leaves for the tiles' box, and how
//     well the tiles fill that box;
//   - the smallest and largest tile, overlaps, and whether every tile is on
//     screen or one scroll of the spotlight strip away;
//   - whether the meeting itself scrolls (it never should);
//   - controls clipped off the bar or the window, controls folded into More,
//     and whether Mic, Camera, Share and Leave are all on screen;
//   - in spotlight, how far the thumbnail strip sits from centred, and
//     whether your own thumbnail is smaller than anyone else's.
// With --check it exits non-zero when a cell breaks one of the rules in
// galleryLabMeasure.ts `judgeGalleryLab` (the dev route and
// tests/galleryLabRendered.test.ts judge by the same function).
//
// No LiveKit, no Tauri, no network: everything is local.
//
// Run:  node scripts/verify-native-gallery-matrix.mjs --out /tmp/native-matrix [--check]
//         [--sizes 840x560,300x900,1400x220] [--counts 1,2,4,9] [--modes grid,spotlight]
//         [--no-variants]   (skip the chat / cameras-off / mixed-shape extras)
//       Open <out>/index.html for the contact sheet; <out>/metrics.json has every reading.
//       Any cell is also a URL: <out>/build/gallery-lab.html#w=300&h=900&n=5&mode=spotlight
// For hands-on poking, `npm run dev` in apps/desktop and open /dev/gallery-lab.

import { createRequire } from 'node:module';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const repoRoot = resolve(import.meta.dirname, '..');
const desktopRoot = resolve(repoRoot, 'apps/desktop');
const requireDesktop = createRequire(join(desktopRoot, 'package.json'));

function arg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  return value === undefined || value.startsWith('--') ? true : value;
}

// Window shapes. The narrow and short ones are why this exists: a column of
// faces beside an editor, a bar of faces along the top of the screen.
const DEFAULT_SIZES = [
  '840x560', // the meeting window's default
  '1280x800',
  '1920x1080',
  '520x380', // the smallest the gallery used to allow
  '300x900', // a side column
  '240x700', // the narrowest the gallery allows (GALLERY_MIN)
  '420x1000',
  '600x1000',
  '1400x220', // a bar along the top
  '1100x170',
  '900x160', // the shortest the gallery allows (GALLERY_MIN)
  '900x340'
];

const sizes = String(arg('sizes', DEFAULT_SIZES.join(',')))
  .split(',')
  .map((size) => size.trim().split('x').map(Number))
  .filter(([w, h]) => w > 0 && h > 0);
const counts = String(arg('counts', '1,2,3,4,6,9,12')).split(',').map(Number);
const modes = String(arg('modes', 'grid,spotlight')).split(',');
const variants = !arg('no-variants', false);
const check = Boolean(arg('check', false));
const outDir = resolve(String(arg('out', join(process.cwd(), 'native-gallery-matrix'))));
const buildDir = join(outDir, 'build');
mkdirSync(join(outDir, 'shots'), { recursive: true });

function query(cell) {
  const params = new URLSearchParams({
    w: String(cell.width),
    h: String(cell.height),
    n: String(cell.count),
    off: String(cell.camerasOff ?? 0),
    aspect: cell.aspect ?? '16:9',
    mode: cell.mode,
    chat: cell.chat ? '1' : '0',
    plugins: '1'
  });
  return params.toString();
}

function cellsToRun() {
  const cells = [];
  for (const [width, height] of sizes) {
    for (const count of counts) {
      for (const mode of modes) {
        cells.push({ width, height, count, mode, label: 'base' });
      }
    }
    if (!variants) continue;
    for (const mode of modes) {
      cells.push({ width, height, count: 5, mode, chat: true, label: 'chat' });
      cells.push({ width, height, count: 6, mode, camerasOff: 3, label: 'cameras-off' });
      cells.push({ width, height, count: 5, mode, aspect: 'mixed', label: 'mixed-cameras' });
    }
  }
  return cells;
}

function cellName(cell) {
  const extra = cell.label === 'base' ? '' : `-${cell.label}`;
  return `${cell.width}x${cell.height}-n${cell.count}-${cell.mode}${extra}`;
}

function html(results) {
  const rows = results
    .map(({ cell, reading, problems }) => {
      const name = cellName(cell);
      const status = problems.length ? `<b class="bad">${problems.map(escape).join('<br>')}</b>` : '<span class="ok">ok</span>';
      return `<figure class="${problems.length ? 'fail' : ''}"><img loading="lazy" src="shots/${name}.png" style="max-width:${Math.min(cell.width, 640)}px"><figcaption><code>${name}</code> · tiles ${(reading.tileShare * 100).toFixed(0)}% · box ${(reading.surfaceShare * 100).toFixed(0)}% · packed ${(reading.packing * 100).toFixed(0)}% · video ${(reading.videoShare * 100).toFixed(0)}% · smallest ${Math.round(reading.smallestTile.width)}x${Math.round(reading.smallestTile.height)} · ${escape(reading.galleryClasses.join(' '))}<br>${status}</figcaption></figure>`;
    })
    .join('\n');
  return `<!doctype html><meta charset="utf-8"><title>Native gallery matrix</title>
<style>body{background:#111;color:#ddd;font:13px system-ui;margin:20px}figure{display:inline-block;vertical-align:top;margin:0 14px 18px 0;max-width:660px}img{display:block;border:1px solid #333}figcaption{margin-top:4px}.bad{color:#ff9b7a;font-weight:600}.ok{color:#8fd3a8}.fail img{border-color:#ff9b7a}</style>
<h1>Native gallery matrix</h1><p>${results.length} cells · ${results.filter((r) => r.problems.length).length} with problems</p>${rows}`;
}

function escape(text) {
  return String(text).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
}

async function buildFixture() {
  const { build } = await import(pathToFileURL(requireDesktop.resolve('vite')).href);
  const { svelte, vitePreprocess } = await import(pathToFileURL(requireDesktop.resolve('@sveltejs/vite-plugin-svelte')).href);
  const fixtureRoot = join(desktopRoot, 'tests/fixtures');
  await build({
    root: fixtureRoot,
    configFile: false,
    logLevel: 'warn',
    base: './',
    esbuild: { tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } }) },
    plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
    resolve: {
      alias: {
        $lib: join(desktopRoot, 'src/lib'),
        '$app/environment': join(fixtureRoot, 'sveltekit-environment.ts'),
        '@petal/shared': join(repoRoot, 'shared')
      }
    },
    build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: join(fixtureRoot, 'gallery-lab.html') } }
  });
}

async function main() {
  await buildFixture();
  const { chromium } = requireDesktop('playwright');
  const browser = await chromium.launch({
    headless: true,
    args: ['--allow-file-access-from-files', '--disable-gpu', '--autoplay-policy=no-user-gesture-required']
  });
  const page = await browser.newPage({ viewport: { width: 840, height: 560 }, deviceScaleFactor: 1 });
  page.on('pageerror', (error) => console.error(`[pageerror] ${error.message}`));
  await page.goto(`${pathToFileURL(join(buildDir, 'gallery-lab.html')).href}#${query({ width: 840, height: 560, count: 4, mode: 'grid' })}`);
  await page.waitForFunction(() => document.body.dataset.ready === 'true', undefined, { timeout: 30_000 });

  const results = [];
  for (const cell of cellsToRun()) {
    // One spare pixel each way, with the pointer parked in it: outside the
    // meeting window, so the cell shows its resting state (a short window's
    // top bar only appears while the pointer is over it).
    await page.setViewportSize({ width: cell.width + 1, height: cell.height + 1 });
    await page.mouse.move(cell.width, cell.height);
    // The rules live with the measurement (src/lib/dev/galleryLabMeasure.ts
    // judgeGalleryLab), shared with the dev route and the rendered test.
    const { reading, problems } = await page.evaluate((q) => window.__galleryLab.set(q), query(cell));
    const name = cellName(cell);
    await page.locator('[data-lab-window]').screenshot({ path: join(outDir, 'shots', `${name}.png`) });
    results.push({ cell, reading, problems });
    const flag = problems.length ? `  ✗ ${problems.join('; ')}` : '';
    console.log(`${name.padEnd(42)} tiles ${(reading.tileShare * 100).toFixed(0).padStart(3)}%  box ${(reading.surfaceShare * 100).toFixed(0).padStart(3)}%  packed ${(reading.packing * 100).toFixed(0).padStart(3)}%  smallest ${Math.round(reading.smallestTile.width)}x${Math.round(reading.smallestTile.height)}${flag}`);
  }
  await browser.close();

  writeFileSync(join(outDir, 'metrics.json'), JSON.stringify(results, null, 2));
  writeFileSync(join(outDir, 'index.html'), html(results));
  const failed = results.filter((r) => r.problems.length);
  console.log(`\n${results.length} cells, ${failed.length} with problems. Contact sheet: ${join(outDir, 'index.html')}`);
  if (check && failed.length) process.exit(1);
}

main().catch((error) => {
  console.error(error);
  process.exit(2);
});
