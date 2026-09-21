// The shared ChatDrawer at the width both clients give it (320 px, and the
// 60%-of-400px column the desktop's minimum window leaves): long names
// ellipsize, unbroken tokens wrap, nothing scrolls horizontally, consecutive
// lines from one sender group under one name, Enter sends and Shift+Enter
// does not, the empty/over-limit composer cannot send, Escape closes.
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

test('chat drawer fits its column, groups senders, and sends on Enter', { timeout: 90_000 }, async () => {
  const buildDir = await mkdtemp(join(tmpdir(), 'petal-chat-drawer-build-'));
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
      build: { outDir: buildDir, emptyOutDir: true, rollupOptions: { input: fileURLToPath(new URL('./chat-drawer.html', fixtureRoot)) } },
    });

    browser = await chromium.launch({ headless: true, args: ['--allow-file-access-from-files', '--disable-gpu'] });

    for (const width of [400, 720]) {
      const page = await browser.newPage({ viewport: { width, height: 600 } });
      await page.goto(pathToFileURL(join(buildDir, 'chat-drawer.html')).href);
      await page.waitForFunction(() => document.body.dataset.ready === 'true');

      const fit = await page.evaluate(() => {
        const aside = document.querySelector('[data-testid="chat-aside"]') as HTMLElement;
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        const overflowing = [...document.querySelectorAll<HTMLElement>('.chat-drawer *')].filter(
          (el) => el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX !== 'hidden' && !el.matches('textarea'),
        );
        return {
          asideWidth: aside.getBoundingClientRect().width,
          listScrollsX: list.scrollWidth > list.clientWidth + 1,
          overflowing: overflowing.map((el) => el.className),
          names: [...document.querySelectorAll<HTMLElement>('.chat-name')].map((el) => ({
            text: el.textContent,
            ellipsized: el.scrollWidth > el.clientWidth,
          })),
          messages: document.querySelectorAll('[data-testid="chat-msg"]').length,
          namedLines: document.querySelectorAll('.chat-meta').length,
          relayed: document.querySelectorAll('[data-testid="chat-msg"][data-relayed="true"]').length,
          times: [...document.querySelectorAll<HTMLElement>('.chat-time')].map((el) => el.textContent),
        };
      });
      assert.ok(fit.asideWidth <= 320 && fit.asideWidth >= 200, `${width}: aside ${fit.asideWidth}`);
      assert.equal(fit.listScrollsX, false, `${width}: the list must never scroll horizontally`);
      assert.deepEqual(fit.overflowing, [], `${width}: no element overflows its box`);
      assert.equal(fit.messages, 4);
      assert.equal(fit.namedLines, 3, 'Theo\'s two consecutive lines share one name line');
      assert.equal(fit.relayed, 1);
      assert.equal(fit.names[0].ellipsized, true, 'a very long name ellipsizes instead of wrapping or pushing the time out');
      assert.deepEqual(fit.names.map((n) => n.text), ['Mira Aleksandra Konstantinopoulou-Whitfield', 'Theo', 'You']);
      assert.ok(fit.times.every((t) => /\d/.test(t ?? '')), `times render: ${fit.times.join(', ')}`);

      // Composer: empty cannot send; Shift+Enter inserts a line; Enter sends
      // the normalized text and clears; the new message appears at the bottom.
      const input = page.locator('[data-testid="chat-input"]');
      const send = page.locator('[data-testid="chat-send"]');
      assert.equal(await send.isDisabled(), true);
      await input.click();
      await page.keyboard.type('  first line');
      await page.keyboard.press('Shift+Enter');
      await page.keyboard.type('second line  ');
      assert.equal(await send.isDisabled(), false);
      assert.equal(await input.inputValue(), '  first line\nsecond line  ');
      await page.keyboard.press('Enter');
      const sent = await page.evaluate(() => (window as unknown as { __chat: { sent: string[] } }).__chat.sent);
      assert.deepEqual(sent, ['first line\nsecond line']);
      assert.equal(await input.inputValue(), '', 'composer clears after send');
      assert.equal(await page.locator('[data-testid="chat-msg"]').count(), 5);
      const last = await page.locator('[data-testid="chat-msg"]').last().locator('.chat-text').textContent();
      assert.equal(last, 'first line\nsecond line');
      const pinned = await page.evaluate(() => {
        const list = document.querySelector('[data-testid="chat-list"]') as HTMLElement;
        return list.scrollHeight - list.scrollTop - list.clientHeight < 2;
      });
      assert.equal(pinned, true, 'the list follows the newest message');

      // Over the limit: the counter warns and Send is disabled; Escape closes.
      await input.fill('x'.repeat(2001));
      assert.equal(await send.isDisabled(), true);
      assert.equal(await page.locator('.chat-count.over').textContent(), '-1');
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => (window as unknown as { __closed: () => number }).__closed()), 1);
      await page.close();
    }
  } finally {
    await browser?.close();
    await rm(buildDir, { recursive: true, force: true });
  }
});
