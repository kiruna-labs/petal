import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';
import { build } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

const repoRoot = resolve(import.meta.dirname, '../..');
const webRoot = resolve(repoRoot, 'web-harness');
const { chromium } = createRequire(import.meta.url)(resolve(repoRoot, 'apps/desktop/node_modules/playwright'));

test('browser meeting device picker dismisses on an action-bar click without consuming it', { timeout: 60_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-browser-popup-dismiss-build-'));
  let browser: Awaited<ReturnType<typeof chromium.launch>> | undefined;

  try {
    await build({
      root: webRoot,
      configFile: false,
      logLevel: 'silent',
      base: './',
      plugins: [svelte()],
      define: {
        __PETAL_BUILD_INFO__: JSON.stringify({ version: 'test', commit: 'test', buildDate: '2099-01-01' }),
        'import.meta.env.VITE_SENTRY_DSN': JSON.stringify('')
      },
      resolve: { alias: { '@petal/shared': resolve(repoRoot, 'shared') } },
      build: {
        outDir: buildDir,
        emptyOutDir: true,
        rollupOptions: { input: resolve(webRoot, 'index.html') }
      }
    });

    browser = await chromium.launch({
      headless: true,
      args: ['--no-sandbox', '--disable-gpu', '--allow-file-access-from-files']
    });
    const page = await browser.newPage({ viewport: { width: 900, height: 700 } });
    await page.goto(pathToFileURL(join(buildDir, 'index.html')).href, { waitUntil: 'load' });
    await page.waitForSelector('#ctl-audio-options', { state: 'attached' });
    await page.evaluate(() => {
      document.querySelector('#meeting-screen')?.classList.remove('hidden');
      document.querySelector('#join-screen')?.classList.add('hidden');
    });

    const audioOptions = page.locator('#ctl-audio-options');
    const share = page.locator('#ctl-share');
    await audioOptions.click();
    await page.waitForFunction(() => !document.querySelector('#devices-menu')?.hasAttribute('hidden'));
    await share.click();
    await page.waitForTimeout(50);

    assert.equal(await page.locator('#devices-menu').getAttribute('hidden'), '');
    assert.equal(await page.evaluate(() => document.activeElement?.id), 'ctl-share');
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
