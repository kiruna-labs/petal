// Built-in plugins must load on desktop even though the route learns the
// host version AFTER PluginSurfaces mounts (getVersion() is async). The PR #4
// review found Reactions never loaded: the route seeded '0.0.0', the
// component booted in onMount against it, hostCompatibility failed, and the
// numeric-version escape treated the placeholder as real. This drives the
// real component + real built-ins in Chromium, not a pure helper.
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { chromium } from 'playwright';
import { build } from 'vite';

const desktopRoot = new URL('..', import.meta.url);
const fixtureRoot = new URL('./fixtures/', import.meta.url);

test('PluginSurfaces loads the built-ins once the host version arrives after mount', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-surfaces-build-'));
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
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./plugin-surfaces.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });
    const page = await browser.newPage({ viewport: { width: 400, height: 700 } });
    const warnings: string[] = [];
    page.on('console', (msg) => {
      if (msg.type() === 'warning' || msg.type() === 'error') warnings.push(msg.text());
    });
    await page.goto(pathToFileURL(join(buildDir, 'plugin-surfaces.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');

    // Mounted with the version unknown: nothing boots, nothing is skipped.
    await page.waitForTimeout(100);
    assert.equal(await page.locator('iframe[data-plugin-frame="logic"]').count(), 0, 'no logic frame before the version is known');
    assert.deepEqual(await page.evaluate(() => (window as any).pluginSurfacesFixture.buttons()), []);
    assert.deepEqual(warnings.filter((w) => w.includes('skipped')), [], 'the unknown version must not be judged incompatible');

    // The route's getVersion() resolves: the built-in Reactions plugin boots.
    await page.evaluate(() => (window as any).pluginSurfacesFixture.setHostVersion('0.9.7'));
    await page.waitForFunction(() => document.querySelector('iframe[data-plugin-frame="logic"][data-plugin-id="petal.reactions"]') !== null);
    const buttons = await page.evaluate(() => (window as any).pluginSurfacesFixture.buttons());
    assert.equal(buttons.length, 1);
    assert.equal(buttons[0].pluginId, 'petal.reactions');
    assert.equal(buttons[0].label, 'React');
    assert.deepEqual(warnings.filter((w) => w.includes('skipped') || w.includes('failed to start')), [], `plugin warnings: ${warnings.join('\n')}`);

    // A later version change never double-loads: still exactly one logic frame.
    await page.evaluate(() => (window as any).pluginSurfacesFixture.setHostVersion('0.9.8'));
    await page.waitForTimeout(100);
    assert.equal(await page.locator('iframe[data-plugin-frame="logic"]').count(), 1);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
