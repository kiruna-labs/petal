// #487 + CLAUDE.md "UI text must never truncate": the meeting window's LIVE
// notice in its longest states (12-hour stream, 50 Mbps, six-digit dropped
// frames, no audio output, reconnecting, "Be right back" countdown, and a
// guest's "Keep me off live streams" switch), rendered at the 400px main
// window and its 380px minimum: every text fits and nothing leaves the window.
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

type Text = {
  tag: string;
  className: string;
  text: string;
  left: number;
  right: number;
  scrollWidth: number;
  clientWidth: number;
  textOverflow: string;
};

type Measurement = {
  viewport: { width: number; deviceScaleFactor: number };
  fonts: string;
  documentScrollWidth: number;
  banner: Text;
  texts: Text[];
};

const SCENARIOS = {
  live: /^LIVE 1[12]:[0-5]\d:[0-5]\d · 49\.9 Mbps · 123456 dropped · No audio output$/,
  paused: /^LIVE 1[12]:[0-5]\d:[0-5]\d · Be right back · ends in 120s · No audio output$/,
  reconnecting: /^Reconnecting \(attempt 3 of 3\)…$/,
  connecting: /^Connecting to the streaming server…$/,
  roomPaused: /^LIVE · Be right back$/,
  optOut: /^LIVE 1[12]:[0-5]\d:[0-5]\d$/
} as const;

test('the LIVE notice fits the 400px main window in its longest states (#487)', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-live-banner-build-'));
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
          '$app/env': fileURLToPath(new URL('./sveltekit-environment.ts', fixtureRoot)),
          '@petal/shared': fileURLToPath(new URL('../../shared', desktopRoot))
        }
      },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: fileURLToPath(new URL('./live-stream-banner.html', fixtureRoot)) }
      }
    });
    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files', '--force-device-scale-factor=1']
    });
    const fixtureUrl = pathToFileURL(join(buildDir, 'live-stream-banner.html')).href;
    for (const width of [400, 380]) {
      for (const [scenario, line] of Object.entries(SCENARIOS)) {
        const where = `${scenario} at ${width}px`;
        const m = await render(browser, `${fixtureUrl}?scenario=${scenario}`, width);
        assert.equal(m.viewport.width, width, `${where}: viewport drifted`);
        assert.equal(m.viewport.deviceScaleFactor, 1);
        assert.equal(m.fonts, 'loaded', `${where}: fonts did not load`);
        const status = m.texts.find((t) => t.className.includes('text'));
        assert.ok(status, `${where}: no status line`);
        assert.match(status.text, line, `${where}: status line`);
        const control = scenario === 'optOut' ? 'Keep me off live streams' : scenario === 'roomPaused' ? null : 'Stop';
        if (control) assert.ok(m.texts.some((t) => t.text === control), `${where}: "${control}" rendered`);
        for (const t of [m.banner, ...m.texts]) {
          const label = `${where}: ${t.tag}.${t.className} "${t.text}"`;
          assert.notEqual(t.textOverflow, 'ellipsis', `${label} would ellipsize`);
          assert.ok(t.scrollWidth <= t.clientWidth, `${label} is truncated (${t.scrollWidth} > ${t.clientWidth})`);
          assert.ok(t.left >= -0.5 && t.right <= width + 0.5, `${label} leaves the window (${t.left}..${t.right})`);
        }
        assert.ok(m.documentScrollWidth <= width, `${where}: the window scrolls sideways`);
      }
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});

async function render(
  browser: Awaited<ReturnType<typeof chromium.launch>>,
  url: string,
  width: number
): Promise<Measurement> {
  const context = await browser.newContext({ viewport: { width, height: 400 }, deviceScaleFactor: 1 });
  const page = await context.newPage();
  try {
    await page.goto(url);
    await page.waitForFunction(
      () => !!document.body.dataset.bannerMeasurement || !!document.body.dataset.bannerMeasurementError,
      undefined,
      { timeout: 15_000 }
    );
    const state = (await page.evaluate(`(() => ({
      measurement: document.body.dataset.bannerMeasurement ?? null,
      error: document.body.dataset.bannerMeasurementError ?? null
    }))()`)) as { measurement: string | null; error: string | null };
    if (state.error) throw new Error(`banner fixture failed: ${decodeURIComponent(state.error)}`);
    if (!state.measurement) throw new Error('banner fixture recorded no measurement');
    return JSON.parse(decodeURIComponent(state.measurement)) as Measurement;
  } finally {
    await context.close();
  }
}
