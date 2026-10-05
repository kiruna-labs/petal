// Plugin provenance at the real 400px width: plugin controls look like every
// other control -- no puzzle badge (owner call) -- while the button's tooltip
// names the plugin AND where it came from, right-click opens the plugin menu
// (not the app-wide editing menu), the menu's copy never clips, and "Turn
// off" reports the plugin id.
//
// The tooltip assertions HIT-TEST. `getAttribute('title')` cannot tell a
// visible tooltip from an unreachable one (kiruna-labs/petal#71 review,
// finding 2), so point at the control and ask what the browser would show.
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

    // No badge and no puzzle glyph anywhere on a plugin control.
    assert.equal(await page.locator('.plugin-provenance').count(), 0);
    assert.equal(await page.locator('.plugin-cell').count(), 3);
    // Two plugins are both called "Reactions": only the host-owned source
    // separates them, so the tooltip has to carry it.
    assert.deepEqual(await page.locator('.plugin-cell button.plugin-button').evaluateAll((els) => els.map((el) => el.getAttribute('title'))), [
      'Reactions · built-in plugin',
      'Reactions · installed plugin',
      'Webhook Notifier Deluxe · installed plugin',
    ]);

    // Reachability, not just presence: point at the control and ask what
    // tooltip the browser would actually show.
    const hits = await page.evaluate(
      (selectors: Record<string, string>) => {
        // No named inner functions here (see the note on the right-click probe below).
        const out: Record<string, unknown> = {};
        for (const [key, selector] of Object.entries(selectors)) {
          const el = document.querySelector<HTMLElement>(selector)!;
          const r = el.getBoundingClientRect();
          const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2) as HTMLElement | null;
          let node: HTMLElement | null = hit;
          let title: string | null = null;
          while (node) {
            if (node.hasAttribute('title')) {
              title = node.getAttribute('title');
              break;
            }
            node = node.parentElement;
          }
          out[key] = {
            computedPointerEvents: getComputedStyle(el).pointerEvents,
            hitIsInside: !!hit && (hit === el || el.contains(hit)),
            titleTheBrowserWouldShow: title,
          };
        }
        return out;
      },
      {
        button: '[data-plugin="petal.reactions"] button.plugin-button',
        countBadge: '[data-plugin="acme.webhook-notifier-pro"] .badge',
      },
    );
    assert.deepEqual(
      hits.button,
      { computedPointerEvents: 'auto', hitIsInside: true, titleTheBrowserWouldShow: 'Reactions · built-in plugin' },
      `the plugin control must explain itself, got ${JSON.stringify(hits.button)}`,
    );
    // The count badge still wins its own pixels and still explains itself.
    assert.deepEqual(
      hits.countBadge,
      { computedPointerEvents: 'auto', hitIsInside: true, titleTheBrowserWouldShow: 'Webhook Notifier Deluxe · installed plugin' },
      `the count badge must stay on top of its own area, got ${JSON.stringify(hits.countBadge)}`,
    );

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
    assert.equal((await menu.locator('.plugin-menu-label').textContent())?.trim(), 'Webhook Notifier Deluxe · installed plugin');
    assert.equal(await menu.locator('.plugin-menu-label svg').count(), 0, 'the menu heading is text only');
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
