// #487 + CLAUDE.md "UI text must never truncate": renders the real go-live
// dialog at its 400px minimum width (and its 460px default) with every hint
// it can show, and asserts every text element fits (scrollWidth <=
// clientWidth) and nothing scrolls sideways. Also pins the plain-rtmp://
// warning: shown for a new or saved rtmp:// destination, not for rtmps://.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { build } from 'vite';
import { chromium } from 'playwright';
import { STREAM_CLEARTEXT_URL_HINT } from '../src/lib/data/liveStream.ts';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

type Text = {
  tag: string;
  className: string;
  text: string;
  left: number;
  right: number;
  width: number;
  height: number;
  scrollWidth: number;
  clientWidth: number;
  scrollHeight: number;
  clientHeight: number;
  textOverflow: string;
  visibility: string;
  display: string;
  opacity: string;
  color: string;
  background: string;
};

type Measurement = {
  viewport: { width: number; deviceScaleFactor: number };
  fonts: string;
  documentScrollWidth: number;
  main: Text;
  texts: Text[];
  selects: { text: string; textWidth: number; room: number }[];
  warnings: Text[];
  afterChange: string[];
  urlInputType: string | null;
};

function assertVisible(t: Text, label: string) {
  assert.ok(t.width > 0 && t.height > 0, `${label} has no box`);
  assert.notEqual(t.display, 'none', `${label} is display:none`);
  assert.equal(t.visibility, 'visible', `${label} is hidden`);
  assert.equal(t.opacity, '1', `${label} is transparent`);
  assert.notEqual(t.color, t.background, `${label} text matches its background`);
}

test('the go-live dialog fits 400px and warns about plain rtmp:// (#487)', { timeout: 120_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-go-live-build-'));
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
        rollupOptions: { input: fileURLToPath(new URL('./go-live.html', fixtureRoot)) }
      }
    });
    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files', '--force-device-scale-factor=1']
    });
    const fixtureUrl = pathToFileURL(join(buildDir, 'go-live.html')).href;

    for (const width of [400, 460]) {
      for (const scenario of ['new', 'saved', 'help'] as const) {
        const where = `${scenario} destination at ${width}px`;
        const m = await render(browser, `${fixtureUrl}?scenario=${scenario}`, width);
        assert.equal(m.viewport.width, width, `${where}: viewport drifted`);
        assert.equal(m.viewport.deviceScaleFactor, 1);
        assert.equal(m.fonts, 'loaded', `${where}: fonts did not load`);
        assert.deepEqual(
          m.warnings.map((w) => w.text),
          scenario === 'help' ? [] : [STREAM_CLEARTEXT_URL_HINT],
          `${where}: the rtmp:// warning`
        );
        for (const warning of m.warnings) assertVisible(warning, `${where}: the warning`);
        assert.deepEqual(m.afterChange, [], `${where}: the warning goes once the URL is rtmps://`);
        assert.ok(m.texts.length > 15, `${where}: the dialog rendered (${m.texts.length} texts)`);
        if (scenario === 'saved') {
          assert.equal(m.selects[0]?.text, 'Team channel (backup ingest)', `${where}: the saved destination`);
        } else {
          const expected =
            scenario === 'new'
              ? 'Save this destination once the stream connects'
              : 'Where do I find my Facebook Live stream key?';
          assert.ok(m.texts.some((t) => t.text.includes(expected)), `${where}: "${expected}" rendered`);
        }
        for (const select of m.selects) {
          assert.ok(
            select.textWidth <= select.room,
            `${where}: select "${select.text}" is clipped (${select.textWidth} > ${select.room})`
          );
        }
        assert.ok(m.texts.some((t) => t.text.includes('fps')), `${where}: the preview and hints rendered`);
        for (const t of m.texts) {
          const label = `${where}: ${t.tag}.${t.className} "${t.text.slice(0, 60)}"`;
          assert.notEqual(t.textOverflow, 'ellipsis', `${label} would ellipsize`);
          assert.ok(t.scrollWidth <= t.clientWidth, `${label} is truncated (${t.scrollWidth} > ${t.clientWidth})`);
          assert.ok(t.scrollHeight <= t.clientHeight + 1, `${label} clips vertically (${t.scrollHeight} > ${t.clientHeight})`);
          assert.ok(t.right <= width + 0.5, `${label} runs past the window (${t.right} > ${width})`);
          assert.ok(t.left >= -0.5, `${label} starts off the left edge`);
        }
        assert.ok(m.main.scrollWidth <= m.main.clientWidth, `${where}: the dialog scrolls sideways`);
        assert.ok(m.documentScrollWidth <= width, `${where}: the window scrolls sideways`);
      }
    }
    for (const scenario of ['new', 'saved'] as const) {
      const m = await render(
        browser,
        `${fixtureUrl}?scenario=${scenario}&url=${encodeURIComponent('rtmps://live.example.com/app')}`,
        400
      );
      assert.deepEqual(m.warnings, [], `${scenario}: no warning for rtmps://`);
    }
    // Streamer mode masks the URL field; the warning still shows.
    const masked = await render(browser, `${fixtureUrl}?scenario=new&streamerMode=1`, 400);
    assert.equal(masked.urlInputType, 'password', 'streamer mode masks the server URL');
    assert.deepEqual(masked.warnings.map((w) => w.text), [STREAM_CLEARTEXT_URL_HINT]);
    for (const warning of masked.warnings) assertVisible(warning, 'streamer mode: the warning');
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
  const context = await browser.newContext({ viewport: { width, height: 660 }, deviceScaleFactor: 1 });
  const page = await context.newPage();
  try {
    await page.goto(url);
    await page.waitForFunction(
      () => !!document.body.dataset.goLiveMeasurement || !!document.body.dataset.goLiveMeasurementError,
      undefined,
      { timeout: 15_000 }
    );
    const state = (await page.evaluate(`(() => ({
      measurement: document.body.dataset.goLiveMeasurement ?? null,
      error: document.body.dataset.goLiveMeasurementError ?? null
    }))()`)) as { measurement: string | null; error: string | null };
    if (state.error) throw new Error(`go-live fixture failed: ${decodeURIComponent(state.error)}`);
    if (!state.measurement) throw new Error('go-live fixture recorded no measurement');
    return JSON.parse(decodeURIComponent(state.measurement)) as Measurement;
  } finally {
    await context.close();
  }
}
