// Plugin provenance at the real 400px width: every host-drawn plugin control
// carries the badge with the plugin's name as its tooltip, right-click opens
// the plugin menu (not the app-wide editing menu), the menu's copy never
// clips, and "Turn off" reports the plugin id.
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

test('plugin controls show provenance and right-click offers turning the plugin off', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-plugin-toolbar-build-'));
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
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./plugin-toolbar.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });
    const page = await browser.newPage({ viewport: { width: 400, height: 600 } });
    await page.goto(pathToFileURL(join(buildDir, 'plugin-toolbar.html')).href);
    await page.waitForFunction(() => document.body.dataset.ready === 'true');

    const badges = page.locator('.plugin-provenance');
    assert.equal(await badges.count(), 2);
    assert.deepEqual(await badges.evaluateAll((els) => els.map((el) => el.getAttribute('title'))), ['Reactions plugin', 'Webhook Notifier Deluxe plugin']);
    // The count badge (top-right) and the provenance badge (bottom-right) must not overlap.
    const boxes = await page.locator('[data-plugin="acme.webhook-notifier-pro"] .badge, [data-plugin="acme.webhook-notifier-pro"] .plugin-provenance').evaluateAll((els) =>
      els.map((el) => el.getBoundingClientRect().toJSON()),
    );
    assert.ok(boxes[0].bottom <= boxes[1].top + 0.5, `count badge ${JSON.stringify(boxes[0])} overlaps provenance ${JSON.stringify(boxes[1])}`);

    // Right-click opens the plugin menu and suppresses the browser/editing menu.
    const claim = await page.evaluate(() => {
      // dispatchEvent is synchronous: after it returns, the cell has run its
      // handler (preventDefault + stopPropagation), so the event tells us both.
      // (No named inner functions here: tsx's transform would inject an
      // `__name` helper that does not exist inside the page.)
      const state = { reachedWindow: false };
      window.addEventListener('contextmenu', () => void (state.reachedWindow = true), { once: true });
      const ev = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 300, clientY: 40 });
      document.querySelector<HTMLElement>('[data-plugin="acme.webhook-notifier-pro"] button')!.dispatchEvent(ev);
      return { defaultPrevented: ev.defaultPrevented, reachedWindow: state.reachedWindow };
    });
    assert.deepEqual(claim, { defaultPrevented: true, reachedWindow: false }, 'the plugin cell claims the right-click before the app-wide menu sees it');
    const menu = page.locator('.plugin-menu');
    await menu.waitFor();
    assert.equal((await menu.locator('.plugin-menu-label').textContent())?.trim(), 'Webhook Notifier Deluxe · plugin');
    assert.equal((await menu.locator('.plugin-menu-row').textContent())?.trim(), 'Turn off Webhook Notifier Deluxe');
    const overflow = await page.evaluate(() => {
      const bad: string[] = [];
      const w = document.documentElement.clientWidth;
      for (const el of Array.from(document.querySelectorAll<HTMLElement>('.plugin-menu, .plugin-menu-label, .plugin-menu-row, .meeting-control-label'))) {
        if (el.scrollWidth > el.clientWidth + 1) bad.push(`${el.className}: ${el.scrollWidth} > ${el.clientWidth}`);
        const r = el.getBoundingClientRect();
        if (r.right > w + 0.5 || r.left < -0.5) bad.push(`${el.className} outside viewport: ${r.left}..${r.right}`);
      }
      return bad;
    });
    assert.deepEqual(overflow, []);

    await menu.locator('.plugin-menu-row').click();
    assert.deepEqual(await page.evaluate(() => (window as any).__events), ['disable:acme.webhook-notifier-pro']);
    assert.equal(await page.locator('.plugin-menu').count(), 0, 'menu closes after selecting');

    // Escape closes an open menu too.
    await page.locator('[data-plugin="petal.reactions"]').click({ button: 'right' });
    await page.locator('.plugin-menu').waitFor();
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('.plugin-menu').count(), 0);
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
