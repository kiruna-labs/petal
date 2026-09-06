// Settings → Plugins rows at the real 400px main-window width: every string
// stays fully visible (project rule: UI text must never truncate), the toggle
// persists to storage, and the permission disclosure lists plain copy.
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

test('Settings → Plugins rows fit 400px with no clipped text and persist the enable toggle', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-settings-build-'));
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
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./plugin-settings.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });
    const page = await browser.newPage({ viewport: { width: 400, height: 900 } });
    await page.goto(pathToFileURL(join(buildDir, 'plugin-settings.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');

    // Open every permissions disclosure so the longest copy is on screen too.
    for (const btn of await page.locator('.permissions-toggle').all()) await btn.click();
    await page.waitForFunction(() => document.querySelectorAll('.permissions li').length > 0);

    const overflow = await page.evaluate(() => {
      const bad: string[] = [];
      const pageWidth = document.documentElement.clientWidth;
      if (document.documentElement.scrollWidth > pageWidth) bad.push(`page scrollWidth ${document.documentElement.scrollWidth} > ${pageWidth}`);
      for (const el of Array.from(document.querySelectorAll<HTMLElement>('.title, .description, .chip, .version, .permissions li, .permissions-toggle'))) {
        if (el.scrollWidth > el.clientWidth + 1) bad.push(`${el.className}: ${el.scrollWidth} > ${el.clientWidth} "${el.textContent?.trim()}"`);
        const r = el.getBoundingClientRect();
        if (r.right > pageWidth + 0.5) bad.push(`${el.className} extends past the viewport: ${r.right}`);
      }
      return bad;
    });
    assert.deepEqual(overflow, []);

    assert.equal(await page.locator('.row').count(), 2);
    assert.deepEqual(await page.locator('.chip').allTextContents(), ['Built-in', 'Registry']);
    const labels = await page.locator('.permissions li').allTextContents();
    assert.ok(labels.includes('See who is in the meeting'));
    assert.ok(labels.includes('Contact hooks.slack.com'));
    assert.ok(labels.every((l) => !l.includes(':')), 'permission ids never leak as raw strings');

    // Toggle Reactions off -> persisted override.
    const reactionsToggle = page.locator('[data-plugin="petal.reactions"] input[type="checkbox"]');
    assert.equal(await reactionsToggle.isChecked(), true);
    await reactionsToggle.click();
    assert.equal(await reactionsToggle.isChecked(), false);
    const stored = await page.evaluate(() => (window as any).__pluginSettingsStorage.get('petal.plugins.enabled.v1'));
    assert.deepEqual(JSON.parse(stored), { 'petal.reactions': false });
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
