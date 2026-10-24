// #310 + CLAUDE.md "UI text must never truncate": a saved server shows
// "Custom server: <host>" on the main window's join/create surface and in
// the meeting. This renders the real MainMenu and Gallery at the 400px main
// window (and its 380px minimum) and asserts scrollWidth <= clientWidth.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';
import { chromium } from 'playwright';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

const LABEL = 'Custom server: department-of-very-long-internal-names.petal.example.com:8443';

type IndicatorMeasurement = {
  text: string;
  left: number;
  right: number;
  width: number;
  height: number;
  scrollWidth: number;
  clientWidth: number;
  scrollHeight: number;
  clientHeight: number;
  opacity: string;
  visibility: string;
  display: string;
  textOverflow: string;
  whiteSpace: string;
  overflowWrap: string;
};

type Measurement = {
  viewport: { width: number; height: number; deviceScaleFactor: number };
  surface: 'menu' | 'meeting';
  fonts: string;
  documentScrollWidth: number;
  indicators: IndicatorMeasurement[];
  store: {
    fromSettings: string | null;
    fromSettingsPort: string | null;
    fromEnv: string | null;
    fromBuild: string | null;
    fromNone: string | null;
    fromEmpty: string | null;
  };
};

test('custom server indicator fits the 400px main window and stays for the meeting (#310)', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-custom-server-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;

  try {
    await build({
      root: fileURLToPath(fixtureRoot),
      configFile: false,
      logLevel: 'silent',
      base: './',
      esbuild: {
        tsconfigRaw: JSON.stringify({ compilerOptions: { target: 'ES2022', useDefineForClassFields: true } })
      },
      plugins: [svelte({ configFile: false, preprocess: vitePreprocess() })],
      resolve: {
        alias: {
          $lib: fileURLToPath(new URL('./src/lib', desktopRoot)),
          '$app/environment': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          '@petal/shared': fileURLToPath(new URL('../../shared', desktopRoot))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: {
          input: fileURLToPath(new URL('./custom-server.html', fixtureRoot))
        }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files', '--force-device-scale-factor=1']
    });

    const fixtureUrl = pathToFileURL(join(buildDir, 'custom-server.html')).href;

    for (const width of [400, 380]) {
      for (const surface of ['menu', 'meeting'] as const) {
        const measurement = await renderMeasurement(
          browser,
          `${fixtureUrl}?surface=${surface}&label=${encodeURIComponent(LABEL)}`,
          width
        );
        const where = `${surface} at ${width}px`;
        assert.equal(measurement.surface, surface);
        assert.equal(measurement.viewport.width, width, `${where}: viewport drifted`);
        assert.equal(measurement.viewport.deviceScaleFactor, 1, 'pixel test must use CSS-pixel scale 1');
        assert.equal(measurement.fonts, 'loaded', 'document fonts did not finish loading');
        assert.equal(measurement.indicators.length, 1, `${where}: indicator did not render`);
        const indicator = measurement.indicators[0];
        assert.equal(indicator.text, LABEL, `${where}: indicator text was shortened`);
        assert.equal(indicator.opacity, '1', `${where}: indicator is not persistently visible`);
        assert.notEqual(indicator.visibility, 'hidden');
        assert.notEqual(indicator.display, 'none');
        assert.ok(indicator.width > 0 && indicator.height > 0, `${where}: indicator has no box`);
        assert.notEqual(indicator.textOverflow, 'ellipsis', `${where}: indicator would ellipsize`);
        assert.ok(
          indicator.scrollWidth <= indicator.clientWidth,
          `${where}: "${indicator.text}" is truncated (scrollWidth ${indicator.scrollWidth} > clientWidth ${indicator.clientWidth})`
        );
        assert.ok(
          indicator.scrollHeight <= indicator.clientHeight,
          `${where}: indicator clips vertically (scrollHeight ${indicator.scrollHeight} > clientHeight ${indicator.clientHeight})`
        );
        assert.ok(indicator.left >= -0.5, `${where}: indicator starts off the left edge`);
        assert.ok(
          indicator.right <= width + 0.5,
          `${where}: indicator runs past the window edge (right ${indicator.right} > ${width})`
        );
        assert.ok(
          measurement.documentScrollWidth <= width,
          `${where}: the window scrolls horizontally (documentScrollWidth ${measurement.documentScrollWidth})`
        );
        assert.equal(measurement.store.fromSettings, 'Custom server: petal.example.com');
        assert.equal(measurement.store.fromSettingsPort, 'Custom server: petal.example.com:8443');
        assert.equal(measurement.store.fromEnv, null);
        assert.equal(measurement.store.fromBuild, null);
        assert.equal(measurement.store.fromNone, null);
        assert.equal(measurement.store.fromEmpty, null);
      }
    }

    for (const surface of ['menu', 'meeting'] as const) {
      const hidden = await renderMeasurement(browser, `${fixtureUrl}?surface=${surface}`, 400);
      assert.deepEqual(hidden.indicators, [], `${surface} shows an indicator when no custom server is saved`);
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});

async function renderMeasurement(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  url: string,
  width: number
): Promise<Measurement> {
  const context = await browser.newContext({
    viewport: { width, height: 720 },
    deviceScaleFactor: 1
  });
  const page = await context.newPage();
  try {
    await page.goto(url);
    await page.waitForFunction(
      () =>
        !!document.body.dataset.customServerMeasurement ||
        !!document.body.dataset.customServerMeasurementError,
      undefined,
      { timeout: 15_000 }
    );
    const encoded = await page.evaluate(`(() => ({
      measurement: document.body.dataset.customServerMeasurement ?? null,
      error: document.body.dataset.customServerMeasurementError ?? null
    }))()`);
    const state = encoded as { measurement: string | null; error: string | null };
    if (state.error) throw new Error(`custom-server fixture failed: ${decodeURIComponent(state.error)}`);
    if (!state.measurement) throw new Error('custom-server fixture recorded no measurement');
    return JSON.parse(decodeURIComponent(state.measurement)) as Measurement;
  } finally {
    await context.close();
  }
}
