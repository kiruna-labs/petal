// #241, rendered: the Gallery top bar's room name, time, view toggle and
// bug-report button share one control height and one centre line. The centre
// line falls out of flex alignment and line-heights together, so measure the
// REAL component (tests/fixtures/gallery-topbar.js mounts Gallery.svelte) in
// Chromium rather than reading its CSS.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import test from 'node:test';
import { chromium } from 'playwright';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

type Box = { height: number; centre: number };
type TopbarReading = {
  controlHeight: number;
  boxes: Record<'name' | 'time' | 'titleAction' | 'toggle' | 'bug', Box | null>;
};

test('gallery top bar: name, time, view toggle and bug-report button share one height and one centre line (#241)', { timeout: 60_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-gallery-topbar-alignment-build-'));
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
          '@petal/shared': resolve(fileURLToPath(new URL('../../shared', desktopRoot)))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./gallery-topbar.html', fixtureRoot)) }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });

    for (const width of [1280, 760, 400]) {
      const page = await browser.newPage({ viewport: { width, height: 700 } });
      // #title-actions: the copy and rename buttons the meeting route always
      // passes. Their 24px height is what the name and time centre against.
      await page.goto(`${pathToFileURL(join(buildDir, 'gallery-topbar.html')).href}#title-actions`);
      // The fixture records its own measurement once fonts and layout settle.
      await page.waitForFunction(
        () => !!document.body.dataset.galleryTopbarMeasurement || !!document.body.dataset.galleryTopbarMeasurementError,
        undefined,
        { timeout: 15_000 }
      );
      const fixtureError = await page.evaluate(() => document.body.dataset.galleryTopbarMeasurementError ?? null);
      assert.equal(fixtureError, null, fixtureError ? decodeURIComponent(fixtureError) : '');

      // Evaluated as a source string: tsx names inline page functions with a
      // `__name` helper that does not exist in the page.
      const reading = (await page.evaluate(`(() => {
        const box = (selector) => {
          const element = document.querySelector(selector);
          if (!element) return null;
          const rect = element.getBoundingClientRect();
          return { height: rect.height, centre: rect.top + rect.height / 2 };
        };
        return {
          controlHeight: parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--topbar-control-height')),
          boxes: {
            name: box('.room-name'),
            time: box('.elapsed'),
            titleAction: box('.room-title-action'),
            toggle: box('.layout-toggle'),
            bug: box('.report-bug')
          }
        };
      })()`)) as TopbarReading;
      const where = `${width}px: ${JSON.stringify(reading)}`;
      const { controlHeight, boxes } = reading;

      assert.ok(controlHeight > 0, `--topbar-control-height did not resolve at ${where}`);
      for (const [part, box] of Object.entries(boxes)) {
        assert.ok(box && box.height > 0, `${part} did not render at ${where}`);
      }
      assert.equal(boxes.toggle!.height, controlHeight, `view toggle height at ${where}`);
      assert.equal(boxes.bug!.height, controlHeight, `bug-report button height at ${where}`);
      const centres = Object.values(boxes).map((box) => box!.centre);
      assert.ok(
        Math.max(...centres) - Math.min(...centres) <= 1,
        `name, time, title actions, view toggle and bug-report button are off one centre line at ${where}`
      );
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
