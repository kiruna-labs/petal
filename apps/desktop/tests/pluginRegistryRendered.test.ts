// Settings → Plugins → Get plugins at the real 400px width, against the
// contract fixture index served through mocked IPC: verified plugins get an
// Install button, unverified ones a chip, the consent block lists plain
// permission copy, nothing clips, and confirming invokes the Rust install
// command with the exact id/version.
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

test('Get plugins lists the registry, gates on verification, shows consent, and installs by id/version', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-registry-build-'));
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
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./plugin-registry.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });
    const page = await browser.newPage({ viewport: { width: 400, height: 900 } });
    await page.goto(pathToFileURL(join(buildDir, 'plugin-registry.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');
    await page.locator('.row').first().waitFor();

    assert.deepEqual(await page.locator('.row').evaluateAll((els) => els.map((el) => el.getAttribute('data-state'))), ['installable', 'unverified']);
    assert.equal((await page.locator('[data-plugin="acme.unverified-thing"] .chip').textContent())?.trim(), 'Awaiting review');
    assert.equal(await page.locator('[data-plugin="acme.unverified-thing"] button').count(), 0, 'unverified has no install control');

    await page.locator('[data-plugin="petal.test-hello"] button', { hasText: 'Install' }).click();
    await page.locator('.consent').waitFor();
    const perms = await page.locator('.consent .permissions li').allTextContents();
    assert.ok(perms.includes('See who is in the meeting'));
    assert.ok(perms.every((p) => !p.includes(':')), 'permission ids never leak');

    const overflow = await page.evaluate(() => {
      const bad: string[] = [];
      const w = document.documentElement.clientWidth;
      if (document.documentElement.scrollWidth > w) bad.push(`page ${document.documentElement.scrollWidth} > ${w}`);
      for (const el of Array.from(document.querySelectorAll<HTMLElement>('.title, .publisher, .description, .chip, .consent-title, .permissions li, button'))) {
        if (el.scrollWidth > el.clientWidth + 1) bad.push(`${el.className || el.tagName}: ${el.scrollWidth} > ${el.clientWidth} "${el.textContent?.trim()}"`);
        if (el.getBoundingClientRect().right > w + 0.5) bad.push(`${el.className || el.tagName} past viewport`);
      }
      return bad;
    });
    assert.deepEqual(overflow, []);

    await page.locator('.consent button', { hasText: 'Install Hello (test)' }).click();
    await page.waitForFunction(() => (window as any).__installed.length > 0);
    assert.deepEqual(await page.evaluate(() => (window as any).__installed), ['petal.test-hello@1.0.0']);
    const installCall = await page.evaluate(() => (window as any).__calls.find((c: any) => c.command === 'plugin_install_from_registry'));
    assert.deepEqual(installCall.payload, { pluginId: 'petal.test-hello', version: '1.0.0' });
    assert.equal(await page.locator('.consent').count(), 0, 'consent closes after install');
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
